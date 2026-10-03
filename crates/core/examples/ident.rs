//! Prints the device fingerprint (hex) for a LanKVM data directory, creating the identity if it
//! doesn't exist yet. For test scripts that pre-pair instances by writing their trust lists.
//!
//!   cargo run --release -p lankvm-core --example ident -- <data dir>

fn main() {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("usage: ident <data dir>");
        std::process::exit(2);
    };
    match transport::identity::DeviceIdentity::load_or_create(std::path::Path::new(&dir)) {
        Ok(id) => println!("{}", id.fingerprint.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(1);
        }
    }
}
