use tokio::net::TcpStream;
use anyhow::Result;

pub async fn connect(address: &str, port: u16) -> Result<TcpStream> {
    let addr = format!("{}:{}", address, port);
    let stream = TcpStream::connect(addr).await?;
    Ok(stream)
}
