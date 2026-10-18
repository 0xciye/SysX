use crate::receiver::UdpDecodedFrameSource;
use crate::udp::encode_jpeg_frame;
use image::ImageFormat;
use std::net::UdpSocket;
use std::path::PathBuf;
use std::time::Duration;

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    match args.first().map(String::as_str) {
        None | Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("udp-listen") => listen(&args[1..]),
        Some("udp-send-image") => send_image(&args[1..]),
        Some("serve") => serve(&args[1..]),
        Some(command) => Err(format!("unsupported command: {command}").into()),
    }
}

fn send_image(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let path = args
        .first()
        .map(PathBuf::from)
        .ok_or("udp-send-image requires a JPEG file path")?;
    let jpeg = std::fs::read(&path)?;
    let decoded = image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg)?;
    let width = u16::try_from(decoded.width())?;
    let height = u16::try_from(decoded.height())?;
    let host = option(args, "--host").unwrap_or_else(|| "127.0.0.1".into());
    let port = option(args, "--port")
        .unwrap_or_else(|| "4200".into())
        .parse::<u16>()?;
    let timestamp_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos()
        .min(u64::MAX as u128) as u64;
    let packets = encode_jpeg_frame(1, width, height, timestamp_ns, &jpeg)?;
    let socket = UdpSocket::bind("127.0.0.1:0")?;
    for packet in &packets {
        socket.send_to(packet, (host.as_str(), port))?;
    }
    println!(
        "{{\"event\":\"sent\",\"frame_id\":1,\"width\":{width},\"height\":{height},\"chunks\":{}}}",
        packets.len()
    );
    Ok(())
}

fn serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("RECEIVER_IPC_TOKEN")?;
    let data_dir = option(args, "--data-dir")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("data"));
    let host = option(args, "--host").unwrap_or_else(|| "127.0.0.1".into());
    let port = option(args, "--port")
        .unwrap_or_else(|| "0".into())
        .parse::<u16>()?;
    let udp_port = option(args, "--udp-port")
        .unwrap_or_else(|| "4200".into())
        .parse::<u16>()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(crate::service::serve(token, data_dir, host, port, udp_port))?;
    Ok(())
}

fn listen(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let host = option(args, "--host").unwrap_or_else(|| "0.0.0.0".into());
    let port = option(args, "--port")
        .unwrap_or_else(|| "4200".into())
        .parse::<u16>()?;
    let count = option(args, "--count")
        .unwrap_or_else(|| "0".into())
        .parse::<usize>()?;
    let mut receiver = UdpDecodedFrameSource::new(host, port)?;
    receiver.open()?;
    let address = receiver.local_addr()?.ok_or("UDP socket did not open")?;
    println!(
        "{{\"event\":\"listening\",\"host\":\"{}\",\"port\":{}}}",
        address.ip(),
        address.port()
    );
    let mut received = 0usize;
    while count == 0 || received < count {
        if let Some(frame) = receiver.read(Duration::from_secs(1))? {
            received += 1;
            println!(
                "{{\"event\":\"frame\",\"frame_id\":{},\"width\":{},\"height\":{},\"bytes\":{}}}",
                frame.frame_id,
                frame.image.width(),
                frame.image.height(),
                frame.image.as_raw().len()
            );
        }
    }
    let stats = receiver.stats();
    println!(
        "{{\"event\":\"summary\",\"received\":{received},\"invalid_packets\":{},\"dropped_frames\":{},\"expired_frames\":{}}}",
        stats.invalid_packets, stats.dropped_frames, stats.expired_frames
    );
    Ok(())
}

fn option(args: &[String], name: &str) -> Option<String> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].clone())
}

fn print_help() {
    println!("SysX Rust receiver backend\n\nCommands:\n  udp-listen [--host 0.0.0.0] [--port 4200] [--count N]\n  udp-send-image FILE.jpg [--host 127.0.0.1] [--port 4200]\n  serve [--host 127.0.0.1] [--port 0] [--data-dir PATH] [--udp-port 4200]");
}
