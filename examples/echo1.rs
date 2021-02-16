// NB: When changing this file update the crate documentation as well.
use graceful_shutdown::Shutdown;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::{select, spawn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = Shutdown::new();
    let listener = TcpListener::bind("127.0.0.1:8000").await?;
    spawn(shutdown.shutdown_after(signal::ctrl_c()));
    loop {
        select! {
            conn = listener.accept() => match conn {
                Ok((mut conn, _)) => { spawn(shutdown.graceful(async move {
                    let mut buf = [0; 1024];
                    loop {
                        let n = match conn.read(&mut buf).await {
                            Ok(n) if n == 0 => return,
                            Ok(n) => n,
                            Err(e) => {
                                eprintln!("failed to read from socket; err = {:?}", e);
                                return;
                            }
                        };
                        if let Err(e) = conn.write_all(&buf[0..n]).await {
                            eprintln!("failed to write to socket; err = {:?}", e);
                            return;
                        }
                    }
                })); },
                Err(e) => {
                    eprintln!("Error accepting connection; err = {:?}", e);
                    shutdown.shutdown();
                }
            },
            _ = shutdown.initiated() => {
                eprintln!("Starting shutdown");
                break;
            }
        }
    }
    shutdown.await;
    Ok(())
}
