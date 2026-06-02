use futures::future::Fuse;
use futures::FutureExt;
use thouart::Console;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
    eprintln!("test_raw: starting");
    let mut cons = Console::new_stdio(None).await?;
    eprintln!("test_raw: console created");

    // force raw-mode initialization
    cons.write_stdout(&[]).await?;
    eprintln!("test_raw: raw mode initialized");

    let mut buffer = vec![];
    loop {
        let (read_fut, write_fut) = if buffer.is_empty() {
            (cons.read_stdin().fuse(), Fuse::terminated())
        } else {
            (Fuse::terminated(), cons.write_stdout(&buffer).fuse())
        };
        tokio::select! {
            input = read_fut => {
                match input {
                    Some(data) => buffer.extend(data),
                    None => break,
                }
            }
            _ = write_fut => {
                buffer.clear()
            }
        }
    }
    eprintln!("test_raw: exiting");
    Ok(())
}
