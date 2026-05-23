use crate::{config::Config, output::OutputFormat};

pub struct Context {
    pub output: OutputFormat,
    pub profile: String,
    pub config: Config,
}

impl Context {
    pub fn new(output: OutputFormat, profile: String, config: Config) -> Self {
        Self {
            output,
            profile,
            config,
        }
    }
}
