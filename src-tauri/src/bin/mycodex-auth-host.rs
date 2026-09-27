fn main() {
    if let Err(code) = cc_switch_lib::mycodex_host::run(std::env::args_os().skip(1)) {
        eprintln!("{code}");
        std::process::exit(1);
    }
}
