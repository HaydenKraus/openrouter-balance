fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=assets/app.manifest");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }

    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.set_manifest_file("assets/app.manifest");

    // Метаданные VersionInfo.
    res.set("ProductName", "OpenRouter Баланс");
    res.set(
        "FileDescription",
        "Расход и остаток баланса OpenRouter на панели задач",
    );
    res.set("CompanyName", "—");
    res.set("LegalCopyright", "© 2026 OpenRouter Баланс. Лицензия MIT.");
    res.set("OriginalFilename", "openrouter-balance.exe");
    res.set("FileVersion", env!("CARGO_PKG_VERSION"));
    res.set("ProductVersion", env!("CARGO_PKG_VERSION"));

    // Ресурсы на русском (0x0419 = LANG_RUSSIAN). winresource применяет язык ко
    // всему VersionInfo-блоку одним LANGID.
    res.set_language(0x0419);

    if let Err(err) = res.compile() {
        println!("cargo:warning=winresource: не удалось собрать ресурсы: {err}");
    }
}
