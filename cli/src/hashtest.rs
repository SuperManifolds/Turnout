use turnout_core::wyhash_nrc1;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 3 {
        // Mode: patch checksum in an NRC1 file
        let payload = std::fs::read(&args[1]).expect("read payload file");
        let hash = wyhash_nrc1::checksum(&payload);
        let mut nrc = std::fs::read(&args[2]).expect("read nrc1 file");
        nrc[24..32].copy_from_slice(&hash.to_le_bytes());
        std::fs::write(&args[2], &nrc).expect("write nrc1 file");
        println!("Patched checksum in {}: {:#018x}", args[2], hash);
    } else {
        // Mode: verify test vector
        let payload = std::fs::read("/tmp/v226_min_payload.bin").expect("read test vector file");
        let hash = wyhash_nrc1::checksum(&payload);
        let expected = 0xbb18_f922_d5cb_4277_u64;
        println!("Payload: {} bytes", payload.len());
        println!("Our hash:  {hash:#018x}");
        println!("Expected:  {expected:#018x}");
        println!("Match: {}", hash == expected);
    }
}
