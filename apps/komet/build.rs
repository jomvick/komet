fn main() {
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("../../dist/windows/komet.ico");
        res.set("ProductName", "Komet");
        res.set("FileDescription", "Komet - Coding Agent Controller");
        let _ = res.compile();
    }
}
