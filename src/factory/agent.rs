//! Agent process adapters. Each implementation owns only provider-specific argv and
//! prompt-delivery policy; workflow safety rules live outside this module.
use anyhow::{Context, Result, bail};
use std::{env, ffi::OsString};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDelivery {
    Argument,
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInvocation {
    pub executable: OsString,
    pub args: Vec<OsString>,
    pub prompt_delivery: PromptDelivery,
}

pub trait AgentAdapter {
    fn invocation(
        &self,
        model: &str,
        stage: &str,
        title: &str,
        prompt: &str,
    ) -> Result<AgentInvocation>;
}

pub struct OpenCodeAdapter;
pub struct ClaudeCodeAdapter;
pub struct CustomAdapter;

impl AgentAdapter for OpenCodeAdapter {
    fn invocation(
        &self,
        model: &str,
        _stage: &str,
        title: &str,
        prompt: &str,
    ) -> Result<AgentInvocation> {
        if model.is_empty() {
            bail!("OpenCode adapter requires a provider/model id");
        }
        Ok(AgentInvocation {
            executable: "opencode".into(),
            args: [
                "run",
                "--standalone",
                "--model",
                model,
                "--title",
                title,
                prompt,
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            prompt_delivery: PromptDelivery::Argument,
        })
    }
}

impl AgentAdapter for ClaudeCodeAdapter {
    fn invocation(
        &self,
        model: &str,
        stage: &str,
        _title: &str,
        _prompt: &str,
    ) -> Result<AgentInvocation> {
        let mut args: Vec<OsString> = [
            "--print",
            "--output-format",
            "text",
            "--no-session-persistence",
            "--permission-prompts",
            "none",
            "--restricted",
            "--tools",
            if stage == "implement" {
                "Read,Edit,Write"
            } else {
                "Read"
            },
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        if !model.is_empty() {
            args.extend([OsString::from("--model"), model.into()]);
        }
        Ok(AgentInvocation {
            executable: "claude".into(),
            args,
            prompt_delivery: PromptDelivery::Stdin,
        })
    }
}

impl AgentAdapter for CustomAdapter {
    fn invocation(
        &self,
        _model: &str,
        _stage: &str,
        _title: &str,
        _prompt: &str,
    ) -> Result<AgentInvocation> {
        let executable = env::var("TOOLER_AI_CUSTOM_ADAPTER")
            .context("custom adapter requires TOOLER_AI_CUSTOM_ADAPTER")?;
        if executable.trim().is_empty() {
            bail!("TOOLER_AI_CUSTOM_ADAPTER cannot be empty");
        }
        Ok(AgentInvocation {
            executable: executable.into(),
            args: Vec::new(),
            prompt_delivery: PromptDelivery::Stdin,
        })
    }
}

pub fn select(name: &str) -> Result<Box<dyn AgentAdapter>> {
    match name {
        "opencode" => Ok(Box::new(OpenCodeAdapter)),
        "claude" => Ok(Box::new(ClaudeCodeAdapter)),
        "custom" => Ok(Box::new(CustomAdapter)),
        _ => bail!("agent adapter must be opencode, claude, or custom"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_uses_a_standalone_server_and_explicit_model() {
        let invocation = OpenCodeAdapter
            .invocation("openai/model", "spec", "title", "prompt")
            .unwrap();
        assert_eq!(invocation.executable, "opencode");
        assert!(invocation.args.contains(&"--standalone".into()));
        assert_eq!(invocation.prompt_delivery, PromptDelivery::Argument);
    }

    #[test]
    fn claude_is_restricted_without_permission_bypass() {
        let invocation = ClaudeCodeAdapter
            .invocation("sonnet", "implement", "title", "prompt")
            .unwrap();
        assert!(invocation.args.contains(&"--restricted".into()));
        assert!(invocation.args.contains(&"Read,Edit,Write".into()));
        assert!(
            !invocation
                .args
                .iter()
                .any(|arg| arg.to_string_lossy().contains("dangerously-skip"))
        );
        assert_eq!(invocation.prompt_delivery, PromptDelivery::Stdin);
    }

    #[test]
    fn custom_adapter_is_invoked_without_shell_parsing() {
        unsafe { env::set_var("TOOLER_AI_CUSTOM_ADAPTER", "/tmp/agent; touch /tmp/nope") };
        let invocation = CustomAdapter
            .invocation("", "review", "title", "prompt")
            .unwrap();
        assert_eq!(invocation.executable, "/tmp/agent; touch /tmp/nope");
        assert!(invocation.args.is_empty());
        unsafe { env::remove_var("TOOLER_AI_CUSTOM_ADAPTER") };
    }
}
