use crate::{context::Context, oauth, output::OutputFormat};
use anyhow::Result;
use clap::{Args, Subcommand};
use std::time::Duration;

#[derive(Args)]
pub struct OAuthArgs {
    #[command(subcommand)]
    pub subcommand: OAuthSubcommand,
}

#[derive(Subcommand)]
pub enum OAuthSubcommand {
    /// Authorize the active profile in a browser using OAuth2 Authorization Code + PKCE
    Login {
        /// OAuth2 scope to request. Repeat for multiple scopes.
        #[arg(long = "scope")]
        scopes: Vec<String>,
        /// Print the authorization URL but do not open a browser
        #[arg(long)]
        no_open: bool,
        /// Seconds to wait for the browser callback
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
}

pub fn run(args: OAuthArgs, ctx: &Context) -> Result<()> {
    match args.subcommand {
        OAuthSubcommand::Login {
            scopes,
            no_open,
            timeout,
        } => {
            let profile = ctx.config.profile.get(&ctx.profile).ok_or_else(|| {
                anyhow::anyhow!(
                    "No profile named '{}'. Configure it with tooler config first.",
                    ctx.profile
                )
            })?;
            oauth::login(
                &ctx.profile,
                profile,
                &scopes,
                no_open,
                Duration::from_secs(timeout),
            )?;
            if ctx.output == OutputFormat::Json {
                println!(
                    "{}",
                    serde_json::json!({"profile": ctx.profile, "logged_in": true})
                );
            } else {
                println!("OAuth2 login completed for profile '{}'.", ctx.profile);
            }
        }
    }
    Ok(())
}
