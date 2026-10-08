fn main() {
    // Просто сообщаем cargo, что нужно пересобирать при изменении этого файла
    println!("cargo:rerun-if-changed=build.rs");
}