use mp4::{Result, TrackType};
use std::env;
use std::fs::File;
use std::io::BufReader;

fn main() -> Result<()> {
    let path = env::args().nth(1).expect("Usage: muxl <file.mp4>");
    let f = File::open(&path).unwrap();
    let size = f.metadata()?.len();
    let reader = BufReader::new(f);

    let mp4 = mp4::Mp4Reader::read_header(reader, size)?;

    // Print boxes.
    println!("major brand: {}", mp4.ftyp.major_brand);
    println!("timescale: {}", mp4.moov.mvhd.timescale);

    // Use available methods.
    println!("size: {}", mp4.size());

    let mut compatible_brands = String::new();
    for brand in mp4.compatible_brands().iter() {
        compatible_brands.push_str(&brand.to_string());
        compatible_brands.push_str(",");
    }
    println!("compatible brands: {}", compatible_brands);
    println!("duration: {:?}", mp4.duration());

    // Track info.
    for track in mp4.tracks().values() {
        println!(
            "track: #{}({}) {} : {}",
            track.track_id(),
            track.language(),
            track.track_type()?,
            track.box_type()?,
        );
        if track.track_type()? == TrackType::Audio {
            println!("audio bitrate: {}", track.bitrate());
        }
    }
    Ok(())
}
