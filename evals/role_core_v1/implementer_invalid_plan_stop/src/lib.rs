pub struct Settings {
    pub retries: u32,
}

impl Settings {
    pub fn conservative() -> Self {
        Self { retries: 1 }
    }
}
