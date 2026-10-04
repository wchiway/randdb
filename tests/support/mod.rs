use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use randdb::config::{Config, Endpoint};
use serde_json::{Value, json};

pub struct Provider {
    pub base: String,
    pub embedded: Arc<AtomicUsize>,
    pub fail_embedding: Arc<AtomicBool>,
    pub fail_rerank: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Provider {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let embedded = Arc::new(AtomicUsize::new(0));
        let fail_embedding = Arc::new(AtomicBool::new(false));
        let fail_rerank = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (count, fail, rerank, done) = (
            embedded.clone(),
            fail_embedding.clone(),
            fail_rerank.clone(),
            stop.clone(),
        );
        let worker = thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => respond(stream, &count, &fail, &rerank),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(e) => panic!("mock API accept: {e}"),
                }
            }
        });
        Self {
            base,
            embedded,
            fail_embedding,
            fail_rerank,
            stop,
            worker: Some(worker),
        }
    }

    pub fn config(&self, home: &Path) -> Config {
        Config {
            home: home.into(),
            dimensions: 2,
            embedding: Endpoint {
                url: format!("{}/embeddings", self.base).parse().unwrap(),
                model: "test-embedding".into(),
                key: "test-key".into(),
            },
            reranker: Endpoint {
                url: format!("{}/rerank", self.base).parse().unwrap(),
                model: "test-reranker".into(),
                key: "test-key".into(),
            },
        }
    }

    pub fn write_config(&self, home: &Path) {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(home.join(".env"),format!("EMBEDDINGS_API_KEY=test-key\nEMBEDDINGS_BASE_URL={}/embeddings\nEMBEDDINGS_MODEL=test-embedding\nEMBEDDINGS_DIMENSIONS=2\nRERANK_API_KEY=test-key\nRERANK_BASE_URL={}/rerank\nRERANK_MODEL=test-reranker\n",self.base,self.base)).unwrap();
    }
}

impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn respond(mut stream: TcpStream, count: &AtomicUsize, fail: &AtomicBool, rerank: &AtomicBool) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut input = BufReader::new(&mut stream);
    let mut first = String::new();
    if input.read_line(&mut first).unwrap_or(0) == 0 {
        return;
    }
    let mut length = 0;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':')
            && key.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap();
        }
    }
    assert!(length < 1024 * 1024);
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes).unwrap();
    let request: Value = serde_json::from_slice(&bytes).unwrap();
    let (status, body) = if first.contains("/embeddings") {
        if fail.load(Ordering::SeqCst) {
            (400, json!({"error":"deliberate test failure"}))
        } else {
            let inputs = request["input"].as_array().unwrap();
            assert!(inputs.len() <= 16);
            count.fetch_add(inputs.len(), Ordering::SeqCst);
            let data: Vec<_> = inputs
                .iter()
                .enumerate()
                .rev()
                .map(|(i, text)| {
                    let auth = text.as_str().unwrap().contains("authenticate");
                    json!({"index":i,"embedding":if auth {vec![1.0,0.01]} else {vec![0.01,1.0]}})
                })
                .collect();
            (200, json!({"data":data}))
        }
    } else if rerank.load(Ordering::SeqCst) {
        (400, json!({"error":"deliberate test failure"}))
    } else {
        let results:Vec<_>=request["documents"].as_array().unwrap().iter().enumerate().map(|(i,text)| {
            json!({"index":i,"relevance_score":if text.as_str().unwrap().contains("authenticate") {0.95} else {0.4}})
        }).collect();
        (200, json!({"results":results}))
    };
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}
