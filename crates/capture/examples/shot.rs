//! Capture one frame of the primary monitor and write it to shot.jpg.
//! Run with: cargo run -p remotecraft-capture --example shot

fn main() {
    let capturer = match remotecraft_capture::Capturer::primary(80) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not bind capturer: {e}");
            std::process::exit(1);
        }
    };
    println!("monitor size: {}x{}", capturer.width(), capturer.height());
    match capturer.capture() {
        Ok(frame) => {
            let path = "shot.jpg";
            if let Err(e) = std::fs::write(path, &frame.jpeg) {
                eprintln!("write failed: {e}");
                std::process::exit(1);
            }
            println!(
                "captured {}x{}, {} bytes jpeg -> {path}",
                frame.width,
                frame.height,
                frame.jpeg.len()
            );
        }
        Err(e) => {
            eprintln!("capture failed: {e}");
            std::process::exit(1);
        }
    }
}
