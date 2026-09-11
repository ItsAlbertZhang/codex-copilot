//! Interactive choices made before installation writes any files.

use std::io::{BufRead, Write};

use anyhow::{bail, Context, Result};

fn answer(input: &mut impl BufRead, output: &mut impl Write, prompt: &str) -> Result<String> {
    write!(output, "{prompt}")?;
    output.flush()?;
    let mut line = String::new();
    if input
        .read_line(&mut line)
        .context("could not read setup answer")?
        == 0
    {
        bail!("setup input closed; installation cancelled before writing files");
    }
    Ok(line.trim().to_string())
}

/// `models` contains only validated, available CAPI reviewer IDs.
pub fn choose_auto_review(
    input: &mut impl BufRead,
    output: &mut impl Write,
    models: &[String],
) -> Result<Option<String>> {
    writeln!(output)?;
    loop {
        match answer(
            input,
            output,
            "Configure automatic approval review? [y/N]: ",
        )?
        .to_ascii_lowercase()
        .as_str()
        {
            "y" | "yes" => break,
            "" | "n" | "no" => return Ok(None),
            _ => writeln!(output, "Please enter y or n.")?,
        }
    }
    if models.is_empty() {
        bail!("no compatible approval models available; check the CAPI seat and Codex catalog");
    }
    let default = models.iter().position(|m| m == "gpt-5.6-luna").unwrap_or(0);
    writeln!(output, "Available approval models:")?;
    for (index, model) in models.iter().enumerate() {
        writeln!(
            output,
            "  {}. {model}{}",
            index + 1,
            if index == default { " (default)" } else { "" }
        )?;
    }
    loop {
        let value = answer(
            input,
            output,
            &format!("Select approval model [{}]: ", default + 1),
        )?;
        let index = if value.is_empty() {
            Some(default)
        } else {
            value.parse::<usize>().ok().and_then(|n| n.checked_sub(1))
        };
        if let Some(model) = index.and_then(|index| models.get(index)) {
            writeln!(output, "Selected approval model: {model}")?;
            return Ok(Some(model.clone()));
        }
        writeln!(output, "Please enter a number from 1 to {}.", models.len())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn choose(answers: &str) -> (Result<Option<String>>, String) {
        let mut output = Vec::new();
        let result = choose_auto_review(
            &mut answers.as_bytes(),
            &mut output,
            &["gpt-6-astra".into(), "gpt-5.6-luna".into()],
        );
        (result, String::from_utf8(output).unwrap())
    }

    #[test]
    fn declining_or_accepting_the_no_default_skips_model_selection() {
        for input in ["n\n", "NO\n", "\n"] {
            let (result, output) = choose(input);
            assert_eq!(result.unwrap(), None);
            assert!(!output.contains("Available approval models"));
        }
    }

    #[test]
    fn explicit_choice_and_default_select_available_models() {
        assert_eq!(
            choose("yes\n1\n").0.unwrap().as_deref(),
            Some("gpt-6-astra")
        );
        assert_eq!(choose("Y\n\n").0.unwrap().as_deref(), Some("gpt-5.6-luna"));
        let mut output = Vec::new();
        assert_eq!(
            choose_auto_review(&mut &b"y\n\n"[..], &mut output, &["gpt-5.5".into()])
                .unwrap()
                .as_deref(),
            Some("gpt-5.5")
        );
    }

    #[test]
    fn invalid_answers_retry_without_selecting_a_model() {
        let (result, output) = choose("maybe\ny\n0\n3\n-1\ntext\n2\n");
        assert_eq!(result.unwrap().as_deref(), Some("gpt-5.6-luna"));
        assert!(output.contains("Please enter y or n"));
        assert_eq!(output.matches("Please enter a number").count(), 4);
    }

    #[test]
    fn closed_input_and_an_empty_model_list_do_not_silently_install() {
        for input in ["", "y\n", "y\n0\n"] {
            assert!(choose(input)
                .0
                .unwrap_err()
                .to_string()
                .contains("cancelled"));
        }
        let result = choose_auto_review(&mut &b"y\n"[..], &mut Vec::new(), &[]);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no compatible approval models"));
    }
}
