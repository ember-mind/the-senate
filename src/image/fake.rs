use std::sync::Mutex;

use super::{GeneratedImage, ImageBackendError, ImageGenerator, ImageRequest, png};

/// Deterministic backend for tests: the PNG bytes are a pure function of the
/// prompt, so evidence can be checked against what actually landed on disk,
/// and every request is recorded so a test can assert how many vendor calls
/// a scenario really made.
pub struct FakeImageGenerator {
    requests: Mutex<Vec<ImageRequest>>,
    failure: Option<ImageBackendError>,
}

impl Default for FakeImageGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeImageGenerator {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            failure: None,
        }
    }

    /// A backend that fails every request the same way.
    #[must_use]
    pub const fn failing(error: ImageBackendError) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            failure: Some(error),
        }
    }

    /// The bytes this backend returns for `prompt`, so a test can predict a
    /// file's content without going through the generator.
    #[must_use]
    pub fn png_for(prompt: &str) -> Vec<u8> {
        let mut seed = 0xcbf2_9ce4_8422_2325u64;
        for byte in prompt.bytes() {
            seed ^= u64::from(byte);
            seed = seed.wrapping_mul(0x0100_0000_01b3);
        }
        png::synthesize(8, 8, seed)
    }

    /// Every request received so far, in order.
    ///
    /// # Panics
    /// If a previous caller panicked while holding the request log.
    #[must_use]
    pub fn requests(&self) -> Vec<ImageRequest> {
        self.requests.lock().expect("fake generator lock").clone()
    }
}

impl ImageGenerator for FakeImageGenerator {
    fn backend(&self) -> &'static str {
        "fake"
    }

    fn model(&self) -> &'static str {
        "fake-image-v1"
    }

    fn generate(&self, request: &ImageRequest) -> Result<GeneratedImage, ImageBackendError> {
        self.requests
            .lock()
            .expect("fake generator lock")
            .push(request.clone());
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        Ok(GeneratedImage {
            png: Self::png_for(&request.prompt),
            backend: "fake",
            model: self.model().to_owned(),
            response_id: Some(format!("fake-{}", self.requests().len())),
        })
    }
}
