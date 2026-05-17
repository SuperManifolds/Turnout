mod wyhash_nrc1;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 3 {
        // Mode: patch checksum in an NRC1 file
        let payload = std::fs::read(&args[1]).unwrap();
        let hash = wyhash_nrc1::checksum(&payload);
        let mut nrc = std::fs::read(&args[2]).unwrap();
        nrc[24..32].copy_from_slice(&hash.to_le_bytes());
        std::fs::write(&args[2], &nrc).unwrap();
        println!("Patched checksum in {}: {:#018x}", args[2], hash);
    } else {
        // Mode: verify test vector
        let payload = std::fs::read("/tmp/v226_min_payload.bin").unwrap();
        let hash = wyhash_nrc1::checksum(&payload);
        let expected = 0xbb18f922d5cb4277u64;
        println!("Payload: {} bytes", payload.len());
        println!("Our hash:  {:#018x}", hash);
        println!("Expected:  {:#018x}", expected);
        println!("Match: {}", hash == expected);
    }
}
