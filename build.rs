fn main() {
    println!("cargo:rerun-if-changed=Windows.ico");

    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("Windows.ico");
        res.compile().expect("Failed to compile Windows resources");
    }
}
