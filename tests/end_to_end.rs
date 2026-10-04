mod support;

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{atomic::Ordering, mpsc},
    time::Duration,
};

use randdb::engine::{Engine, SearchRequest};
use serde_json::{Value, json};
use support::Provider;
use tokio_util::sync::CancellationToken;

fn request(root: &std::path::Path) -> SearchRequest {
    SearchRequest {
        repo_path: root.canonicalize().unwrap().to_str().unwrap().into(),
        information_request: "How does authenticate validate credentials?".into(),
        technical_terms: vec!["authenticate".into()],
        max_total_chars: Some(1200),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_search_update_delete_and_restart_use_real_stores() {
    let provider = Provider::start();
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let old = "// 中文🦀\r\npub fn authenticate() -> bool { true }\r\n";
    std::fs::write(root.path().join("auth.rs"), old).unwrap();
    std::fs::write(
        root.path().join("other.py"),
        "def unrelated():\n    return 42\n",
    )
    .unwrap();
    let config = provider.config(home.path());
    let mut engine = Engine::open(config.clone(), root.path(), CancellationToken::new())
        .await
        .unwrap();
    let initial = engine.index(false).await.unwrap();
    assert_eq!(initial.indexed, 2);
    let embedded = provider.embedded.load(Ordering::SeqCst);
    let unchanged = engine.index(false).await.unwrap();
    assert_eq!(unchanged.unchanged, 2);
    assert_eq!(provider.embedded.load(Ordering::SeqCst), embedded);
    let output = engine.search(&request(root.path())).await.unwrap();
    assert!(output.contains("auth.rs (L1-"), "{output}");
    assert!(output.contains("中文🦀"));
    assert!(output.chars().count() <= 1200);
    std::fs::write(
        root.path().join("auth.rs"),
        "pub fn authenticate() -> bool { false /* new generation */ }\n",
    )
    .unwrap();
    std::fs::remove_file(root.path().join("other.py")).unwrap();
    drop(engine);
    let mut engine = Engine::open(config, root.path(), CancellationToken::new())
        .await
        .unwrap();
    let report = engine.index(false).await.unwrap();
    assert_eq!((report.indexed, report.removed), (1, 1));
    let output = engine.search(&request(root.path())).await.unwrap();
    assert!(output.contains("new generation"));
    assert!(!output.contains("中文🦀") && !output.contains("other.py"));
    provider.fail_rerank.store(true, Ordering::SeqCst);
    assert!(
        engine
            .search(&request(root.path()))
            .await
            .unwrap()
            .contains("Reranker unavailable")
    );
    std::fs::remove_file(root.path().join("auth.rs")).unwrap();
    assert_eq!(
        engine.search(&request(root.path())).await.unwrap(),
        "No matching indexed code found."
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_failure_unblocks_a_full_pipeline_and_can_be_retried() {
    let provider = Provider::start();
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    for i in 0..24 {
        std::fs::write(
            root.path().join(format!("{i}.rs")),
            format!("fn function_{i}() {{}}\n"),
        )
        .unwrap();
    }
    let mut engine = Engine::open(
        provider.config(home.path()),
        root.path(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    provider.fail_embedding.store(true, Ordering::SeqCst);
    let failure = tokio::time::timeout(Duration::from_secs(10), engine.index(false))
        .await
        .expect("producer deadlock after API failure");
    assert!(failure.is_err());
    provider.fail_embedding.store(false, Ordering::SeqCst);
    assert_eq!(engine.index(false).await.unwrap().indexed, 24);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ignored_and_newly_binary_files_are_retired_without_reembedding() {
    let provider = Provider::start();
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("auth.rs"), "fn authenticate() {}\n").unwrap();
    let mut engine = Engine::open(
        provider.config(home.path()),
        root.path(),
        CancellationToken::new(),
    )
    .await
    .unwrap();
    engine.index(false).await.unwrap();
    std::fs::write(root.path().join("auth.rs"), b"binary\0bytes").unwrap();
    let report = engine.index(false).await.unwrap();
    assert_eq!((report.removed, report.skipped), (1, 1));
    std::fs::write(root.path().join("auth.rs"), "fn authenticate() {}\n").unwrap();
    engine.index(false).await.unwrap();
    std::fs::write(root.path().join(".gitignore"), "auth.rs\n").unwrap();
    assert_eq!(engine.index(false).await.unwrap().removed, 1);
}

#[test]
fn model_identity_changes_cache_even_when_dimensions_match() {
    let provider = Provider::start();
    let home = tempfile::tempdir().unwrap();
    let config = provider.config(home.path());
    let mut changed = config.clone();
    changed.embedding.model = "another-model".into();
    assert_ne!(config.fingerprint(), changed.fingerprint());
    changed = config.clone();
    changed.embedding.key = "rotated-secret".into();
    assert_eq!(config.fingerprint(), changed.fingerprint());
}

struct McpProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl McpProcess {
    fn start(home: &std::path::Path) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_randdb"));
        command.args(["--data-dir", home.to_str().unwrap(), "mcp"]);
        for key in [
            "EMBEDDINGS_API_KEY",
            "EMBEDDINGS_BASE_URL",
            "EMBEDDINGS_MODEL",
            "EMBEDDINGS_DIMENSIONS",
            "RERANK_API_KEY",
            "RERANK_BASE_URL",
            "RERANK_MODEL",
        ] {
            command.env_remove(key);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if send.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            lines,
        }
    }
    fn send(&mut self, value: Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{value}").unwrap();
        self.stdin.as_mut().unwrap().flush().unwrap();
    }
    fn response(&self, id: u64) -> Value {
        loop {
            let line = self
                .lines
                .recv_timeout(Duration::from_secs(30))
                .expect("MCP response timeout or early process exit");
            let value: Value =
                serde_json::from_str(&line).expect("non-JSON log leaked into MCP stdout");
            if value["id"] == id {
                return value;
            }
        }
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn stdio_protocol_exposes_one_tool_and_retrieves_code() {
    let provider = Provider::start();
    let home = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    provider.write_config(home.path());
    std::fs::write(
        root.path().join("auth.rs"),
        "fn authenticate() { /* unique_mcp_marker */ }\n",
    )
    .unwrap();
    let mut process = McpProcess::start(home.path());
    process.send(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"randdb-test","version":"1"}}}));
    assert_eq!(
        process.response(1)["result"]["serverInfo"]["name"],
        "randdb"
    );
    process.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    process.send(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let tools = process.response(2);
    let tools = tools["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "codebase-retrieval");
    assert_eq!(
        tools[0]["inputSchema"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        4
    );
    process.send(json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"codebase-retrieval","arguments":{"repo_path":root.path().canonicalize().unwrap(),"information_request":"Find authenticate","max_total_chars":1000}}}));
    let response = process.response(3);
    assert_ne!(response["result"]["isError"], true, "{response}");
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("unique_mcp_marker") && text.chars().count() <= 1000);
    process.send(json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"codebase-retrieval","arguments":{"repo_path":root.path(),"information_request":"Find code","mode":"deep"}}}));
    let invalid = process.response(4);
    assert!(
        invalid["error"].is_object() || invalid["result"]["isError"] == true,
        "unsupported old parameter was ignored: {invalid}"
    );
}
