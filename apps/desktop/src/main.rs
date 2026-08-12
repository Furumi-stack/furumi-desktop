fn main() -> Result<(), Box<dyn std::error::Error>> {
    let backend = furumi_backend::spawn_backend()?;
    furumi_ui::run(&backend)?;
    Ok(())
}
