//! Example of TCP echo server using `graceful_stream`.
use graceful_shutdown::Shutdown;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::signal;
use tokio::spawn;
use tokio_stream::{wrappers::TcpListenerStream, StreamExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let shutdown = Shutdown::new();
    let listener = TcpListener::bind("127.0.0.1:8000").await?;
    let mut stream = shutdown.graceful_stream(TcpListenerStream::new(listener));
    spawn(shutdown.shutdown_after(signal::ctrl_c()));
    while let Some(conn) = stream.next().await {
        match conn {
            Ok(mut conn) => {
                spawn(shutdown.graceful(async move {
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
                }));
            }
            Err(e) => {
                eprintln!("Error accepting connection; err = {:?}", e);
                shutdown.shutdown();
                break;
            }
        }
    }
    eprintln!("Shutting down");
    shutdown
        .with_timeout(std::time::Duration::from_secs(30))
        .await;
    Ok(())
}
