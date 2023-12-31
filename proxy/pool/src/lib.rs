use common::tracing;
use proxy_common::{
    anyhow,
    bytes::Bytes,
    http_body_util::combinators::BoxBody,
    hyper::{
        self,
        client::conn::http1::{Builder, SendRequest},
    },
    hyper_util::rt::TokioIo,
    tokio::{
        self,
        net::TcpStream,
        sync::{Mutex, MutexGuard},
    },
};
use std::{collections::HashMap, sync::OnceLock};

fn random_id() -> u64 {
    let now = std::time::SystemTime::now();
    let duration = match now.duration_since(std::time::UNIX_EPOCH) {
        Ok(n) => n,
        Err(_) => {
            let random = rand::random::<u64>();
            return random;
        }
    };
    duration.as_secs() * 1_000_000_000 + duration.subsec_nanos() as u64
}

pub struct ManageConnection;
type SendRequestPool = Vec<(u64, Mutex<SendRequest<BoxBody<Bytes, hyper::Error>>>)>;
type SendRequestPoolId = Vec<(String, u64)>;

static mut CONNECTIONS: OnceLock<SendRequestPool> = OnceLock::new();
static mut TEMP_CONNECTIONS: OnceLock<
    HashMap<String, Mutex<SendRequest<BoxBody<Bytes, hyper::Error>>>>,
> = OnceLock::new();

pub fn init() {
    unsafe {
        CONNECTIONS.get_or_init(|| {
            // println!("Init connection");
            tracing::info!("Init connection");
            Vec::new()
        });
        TEMP_CONNECTIONS.get_or_init(|| {
            // println!("Init temp connection");
            tracing::info!("Init temp connection");
            HashMap::new()
        });
    }
}

lazy_static::lazy_static! {
    static ref CONNECTIONS_IDS: Mutex<SendRequestPoolId> = Mutex::new(Vec::new());
}

impl ManageConnection {
    pub async fn get(
        addr: String,
        max_open_connections: u32,
    ) -> Result<MutexGuard<'static, SendRequest<BoxBody<Bytes, hyper::Error>>>, anyhow::Error> {
        let mut conn_ids = CONNECTIONS_IDS.lock().await;
        let conn = unsafe {
            match CONNECTIONS.get_mut() {
                Some(c) => c,
                None => {
                    // sleep 50ms
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                    match CONNECTIONS.get_mut() {
                        Some(c) => c,
                        None => return Err(anyhow::anyhow!("Connection pool is not initialized")),
                    }
                }
            }
        };

        let mut is_max_connections = false;
        if conn_ids.is_empty() {
            let id = random_id();
            conn_ids.push((addr.clone(), id));
            match ManageConnection::new_connection(&mut (addr.clone(), id)).await {
                Ok(_) => {}
                Err(err) => {
                    conn_ids.retain(|(a, i_id)| a != &addr.clone() && i_id != &id.clone());
                    drop(conn_ids);
                    return Err(err);
                }
            }
        } else if conn_ids.iter().filter(|(a, _)| a == &addr).count()
            < max_open_connections as usize
        {
            let id = random_id();
            conn_ids.push((addr.clone(), id));
            match ManageConnection::new_connection(&mut (addr.clone(), id)).await {
                Ok(_) => {}
                Err(err) => {
                    conn_ids.retain(|(a, i_id)| a != &addr.clone() && i_id != &id.clone());
                    drop(conn_ids);
                    return Err(err);
                }
            }
        } else {
            is_max_connections = true;
        }

        if !is_max_connections {
            let temp_conn = unsafe {
                match TEMP_CONNECTIONS.get_mut() {
                    Some(c) => c,
                    None => {
                        // sleep 50ms
                        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                        match TEMP_CONNECTIONS.get_mut() {
                            Some(c) => c,
                            None => {
                                return Err(anyhow::anyhow!("Connection pool is not initialized"))
                            }
                        }
                    }
                }
            };
            let sender = match create_conn(addr.clone()).await {
                Ok(s) => s,
                Err(err) => {
                    drop(conn_ids);
                    return Err(err);
                }
            };
            let sender = Mutex::new(sender);
            temp_conn.insert(addr.clone(), sender);
            if let Some(sender) = temp_conn.get(&addr) {
                let sender = sender.lock().await;
                return Ok(sender);
            }
        }
        let random = rand::random::<usize>() % conn_ids.iter().filter(|(a, _)| a == &addr).count();
        let state = conn_ids
            .iter()
            .filter(|(a, _)| a == &addr)
            .nth(random)
            .unwrap()
            .clone();
        let id = state.1;
        // unlock conn_ids
        drop(conn_ids);
        for (i_id, sender) in conn.iter() {
            if i_id == &id {
                let sender = sender.lock().await;
                if !sender.is_closed() {
                    return Ok(sender);
                }
                drop(sender);
            }
        }
        Err(anyhow::anyhow!("Connection not found"))
    }

    async fn new_connection(row: &mut (String, u64)) -> Result<bool, anyhow::Error> {
        let (addr, id) = row;
        let stream = match TcpStream::connect(addr.clone()).await {
            Ok(s) => s,
            Err(err) => {
                return Err(anyhow::anyhow!("Connection failed: {:?} -> {}", err, addr));
            }
        };
        let io = TokioIo::new(stream);
        let (sender, conn) = match Builder::new()
            .preserve_header_case(true)
            .title_case_headers(true)
            .handshake(io)
            .await
        {
            Ok((s, c)) => (s, c),
            Err(err) => {
                return Err(anyhow::anyhow!("Connection failed: {:?} -> {}", err, addr));
            }
        };

        let conns = unsafe {
            match CONNECTIONS.get_mut() {
                Some(c) => c,
                None => {
                    return Err(anyhow::anyhow!("Connection pool is not initialized"));
                }
            }
        };
        conns.push((*id, Mutex::new(sender)));

        let id = *id;
        let addr = addr.clone();
        tokio::task::spawn(async move {
            if let Err(err) = conn.await {
                println!("Connection failed: {:?}", err);
            }
            let conns = unsafe {
                match CONNECTIONS.get_mut() {
                    Some(c) => c,
                    None => return,
                }
            };
            conns.retain(|(i_id, _)| i_id != &id);
            let mut conn_ids = CONNECTIONS_IDS.lock().await;
            conn_ids.retain(|(a, i_id)| a != &addr && i_id != &id);
            drop(conn_ids);
            println!("Disconnect: {} -> {}", addr, id);
        });
        Ok(true)
    }
}

async fn create_conn(
    addr: String,
) -> Result<SendRequest<BoxBody<Bytes, hyper::Error>>, anyhow::Error> {
    let stream = match TcpStream::connect(addr.clone()).await {
        Ok(s) => s,
        Err(err) => {
            return Err(anyhow::anyhow!("Connection failed: {:?} -> {}", err, addr));
        }
    };
    let io = TokioIo::new(stream);
    let (sender, conn) = match Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .handshake(io)
        .await
    {
        Ok((s, c)) => (s, c),
        Err(err) => {
            return Err(anyhow::anyhow!("Connection failed: {:?} -> {}", err, addr));
        }
    };
    tokio::task::spawn(async move {
        if let Err(err) = conn.await {
            println!("Connection failed: {:?}", err);
        }
        let temp_conn = unsafe {
            match TEMP_CONNECTIONS.get_mut() {
                Some(c) => c,
                None => return,
            }
        };
        temp_conn.remove(&addr);
        println!("Disconnect temp: {}", addr);
    });
    Ok(sender)
}
