use graceful_shutdown::Shutdown;
use smol::{io, net, stream::StreamExt};

fn shutdown_on_eof(shutdown: &Shutdown) {
    let shutdown = shutdown.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut buf = String::new();
        loop {
            match stdin.read_line(&mut buf) {
                Ok(0) | Err(_) => {
                    shutdown.shutdown();
                    break;
                }
                _ => {
                    buf.clear();
                }
            }
        }
    });
}

fn main() -> io::Result<()> {
    smol::block_on(async {
        let shutdown = Shutdown::new();
        let listener = net::TcpListener::bind("127.0.0.1:8000").await?;
        let mut stream = shutdown.graceful_stream(listener.incoming());

        shutdown_on_eof(&shutdown);
        while let Some(conn) = stream.next().await {
            match conn {
                Ok(conn) => {
                    smol::spawn(shutdown.graceful(async move {
                        let (read, write) = io::split(conn);
                        if let Err(e) = io::copy(read, write).await {
                            eprintln!("Error echoing on socket; err = {:?}", e);
                        }
                    }))
                    .detach();
                }
                Err(e) => {
                    eprintln!("Error accepting connection; err = {:?}", e);
                    shutdown.shutdown();
                    break;
                }
            }
        }

        eprintln!("Shutting down");
        // timeout after 30 seconds
        shutdown
            .with_timer(std::time::Duration::from_secs(30))
            .await;
        Ok(())
    })
}
