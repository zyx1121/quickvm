fn main() {
    // Windows：把 quickvm.ico 嵌進 exe（id 1）— 給 .exe 本身的圖示 + tray LoadIcon 用。
    // host==target 原生編譯（mac 原生 / king 原生），故 cfg(windows) 對齊目標平台；不跨編。
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon_with_id("assets/quickvm.ico", "1");
        res.compile().unwrap();
    }
}
