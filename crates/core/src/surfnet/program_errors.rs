use solana_instruction::error::InstructionError;
use solana_pubkey::Pubkey;
use solana_system_interface::error::SystemError;
use solana_transaction_error::TransactionError;
use surfpool_types::Idl;

/// The text of `err`, followed by the name and message of the program error when the program
/// that returned it has the code in its IDL, in an Anchor error log, or in a built-in error table.
pub fn describe_transaction_error(
    err: &TransactionError,
    logs: &[String],
    idl_for: impl Fn(&Pubkey) -> Option<Idl>,
) -> String {
    match name_program_error(err, logs, idl_for) {
        Some(name) => format!("{err} ({name})"),
        None => err.to_string(),
    }
}

fn name_program_error(
    err: &TransactionError,
    logs: &[String],
    idl_for: impl Fn(&Pubkey) -> Option<Idl>,
) -> Option<String> {
    let TransactionError::InstructionError(_, InstructionError::Custom(code)) = err else {
        return None;
    };
    // The program that returned the error logs its failure first, then each program that
    // invoked it logs the same failure.
    let failure = format!(" failed: {}", InstructionError::Custom(*code));
    let (failed_at, program) = logs.iter().enumerate().find_map(|(index, log)| {
        let address = log.strip_prefix("Program ")?.strip_suffix(&failure)?;
        Some((index, address.parse::<Pubkey>().ok()?))
    })?;

    let idl = idl_for(&program);
    let label = idl
        .as_ref()
        .map_or_else(|| program.to_string(), |idl| idl.metadata.name.clone());

    idl.as_ref()
        .and_then(|idl| idl_error(idl, *code))
        .or_else(|| anchor_error(&program, *code, &logs[..failed_at], &label))
        .or_else(|| builtin_error(&program, *code))
}

fn idl_error(idl: &Idl, code: u32) -> Option<String> {
    let error = idl.errors.iter().find(|error| error.code == code)?;
    Some(describe(
        &idl.metadata.name,
        &error.name,
        error.msg.as_deref(),
    ))
}

/// Reads `AnchorError … Error Code: <name>. Error Number: <code>. Error Message: <message>.`,
/// which Anchor programs log before failing.
fn anchor_error(program: &Pubkey, code: u32, logs: &[String], label: &str) -> Option<String> {
    let invoke = format!("Program {program} invoke");
    let frame = &logs[logs.iter().rposition(|log| log.starts_with(&invoke))?..];
    let number = format!("Error Number: {code}.");
    frame.iter().rev().find_map(|log| {
        let log = log.strip_prefix("Program log: AnchorError")?;
        if !log.contains(&number) {
            return None;
        }
        let name = log.split("Error Code: ").nth(1)?.split('.').next()?;
        let message = log
            .split("Error Message: ")
            .nth(1)
            .map(|message| message.trim_end_matches('.'));
        Some(describe(label, name, message))
    })
}

fn builtin_error(program: &Pubkey, code: u32) -> Option<String> {
    if *program == solana_sdk_ids::system_program::ID {
        let error = SystemError::try_from(code).ok()?;
        Some(describe(
            "System Program",
            &format!("{error:?}"),
            Some(&error.to_string()),
        ))
    } else if *program == spl_token_interface::ID {
        let error = spl_token_interface::error::TokenError::try_from(code).ok()?;
        Some(describe(
            "Token Program",
            &format!("{error:?}"),
            Some(&error.to_string()),
        ))
    } else if *program == spl_token_2022_interface::ID {
        let error = spl_token_2022_interface::error::TokenError::try_from(code).ok()?;
        Some(describe(
            "Token-2022 Program",
            &format!("{error:?}"),
            Some(&error.to_string()),
        ))
    } else {
        None
    }
}

fn describe(program: &str, name: &str, message: Option<&str>) -> String {
    match message {
        Some(message) => format!("{program} error {name}: {message}"),
        None => format!("{program} error {name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAULT: Pubkey = Pubkey::new_from_array([7; 32]);

    fn logs(lines: &[&str]) -> Vec<String> {
        lines.iter().map(|line| line.to_string()).collect()
    }

    fn custom(code: u32) -> TransactionError {
        TransactionError::InstructionError(0, InstructionError::Custom(code))
    }

    fn no_idl(_: &Pubkey) -> Option<Idl> {
        None
    }

    fn vault_idl(program: &Pubkey) -> Option<Idl> {
        (*program == VAULT).then(|| {
            serde_json::from_value(serde_json::json!({
                "address": VAULT.to_string(),
                "metadata": { "name": "vault", "version": "0.1.0", "spec": "0.1.0" },
                "instructions": [],
                "errors": [
                    { "code": 1, "name": "VaultLocked" },
                    { "code": 6000, "name": "AmountTooLarge", "msg": "Amount exceeds the vault limit" }
                ]
            }))
            .unwrap()
        })
    }

    #[test]
    fn a_system_transfer_short_of_lamports_is_named() {
        let logs = logs(&[
            "Program 11111111111111111111111111111111 invoke [1]",
            "Transfer: insufficient lamports 1000000000, need 10000000000",
            "Program 11111111111111111111111111111111 failed: custom program error: 0x1",
        ]);

        assert_eq!(
            describe_transaction_error(&custom(1), &logs, no_idl),
            "Error processing Instruction 0: custom program error: 0x1 (System Program error \
             ResultWithNegativeLamports: account does not have enough SOL to perform the operation)"
        );
    }

    #[test]
    fn the_same_code_is_named_after_the_program_that_returned_it() {
        let logs = logs(&[
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [1]",
            "Program log: Instruction: Transfer",
            "Program log: Error: insufficient funds",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA consumed 4381 of 200000 compute units",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA failed: custom program error: 0x1",
        ]);

        assert_eq!(
            describe_transaction_error(&custom(1), &logs, no_idl),
            "Error processing Instruction 0: custom program error: 0x1 (Token Program error \
             InsufficientFunds: Insufficient funds)"
        );
    }

    #[test]
    fn a_registered_idl_names_its_errors() {
        let logs = logs(&[
            &format!("Program {VAULT} invoke [1]"),
            &format!("Program {VAULT} consumed 2100 of 200000 compute units"),
            &format!("Program {VAULT} failed: custom program error: 0x1770"),
        ]);

        assert_eq!(
            describe_transaction_error(&custom(6000), &logs, vault_idl),
            "Error processing Instruction 0: custom program error: 0x1770 (vault error \
             AmountTooLarge: Amount exceeds the vault limit)"
        );
    }

    #[test]
    fn the_innermost_failing_program_is_named() {
        // The vault's IDL has its own error 1, which must not be confused with Token's.
        let logs = logs(&[
            &format!("Program {VAULT} invoke [1]"),
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [2]",
            "Program log: Error: insufficient funds",
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA failed: custom program error: 0x1",
            &format!("Program {VAULT} consumed 9000 of 200000 compute units"),
            &format!("Program {VAULT} failed: custom program error: 0x1"),
        ]);

        assert_eq!(
            describe_transaction_error(&custom(1), &logs, vault_idl),
            "Error processing Instruction 0: custom program error: 0x1 (Token Program error \
             InsufficientFunds: Insufficient funds)"
        );
    }

    #[test]
    fn an_anchor_error_log_names_a_program_without_an_idl() {
        let logs = logs(&[
            &format!("Program {VAULT} invoke [1]"),
            "Program log: Instruction: Withdraw",
            "Program log: AnchorError thrown in programs/vault/src/lib.rs:42. Error Code: \
             AmountTooLarge. Error Number: 6000. Error Message: Amount exceeds the vault limit.",
            &format!("Program {VAULT} consumed 4000 of 200000 compute units"),
            &format!("Program {VAULT} failed: custom program error: 0x1770"),
        ]);

        assert_eq!(
            describe_transaction_error(&custom(6000), &logs, no_idl),
            format!(
                "Error processing Instruction 0: custom program error: 0x1770 ({VAULT} error \
                 AmountTooLarge: Amount exceeds the vault limit)"
            )
        );
    }

    #[test]
    fn an_anchor_framework_error_is_named_after_the_registered_idl() {
        // Anchor's own errors, like 101, are not listed in a program's IDL.
        let logs = logs(&[
            &format!("Program {VAULT} invoke [1]"),
            "Program log: AnchorError occurred. Error Code: InstructionFallbackNotFound. Error \
             Number: 101. Error Message: Fallback functions are not supported.",
            &format!("Program {VAULT} consumed 900 of 200000 compute units"),
            &format!("Program {VAULT} failed: custom program error: 0x65"),
        ]);

        assert_eq!(
            describe_transaction_error(&custom(101), &logs, vault_idl),
            "Error processing Instruction 0: custom program error: 0x65 (vault error \
             InstructionFallbackNotFound: Fallback functions are not supported)"
        );
    }

    #[test]
    fn an_unknown_code_keeps_the_runtime_text() {
        let system = logs(&[
            "Program 11111111111111111111111111111111 invoke [1]",
            "Program 11111111111111111111111111111111 failed: custom program error: 0x63",
        ]);
        let unknown_program = logs(&[
            &format!("Program {VAULT} invoke [1]"),
            &format!("Program {VAULT} failed: custom program error: 0x1"),
        ]);

        assert_eq!(
            describe_transaction_error(&custom(99), &system, no_idl),
            "Error processing Instruction 0: custom program error: 0x63"
        );
        assert_eq!(
            describe_transaction_error(&custom(1), &unknown_program, no_idl),
            "Error processing Instruction 0: custom program error: 0x1"
        );
    }

    #[test]
    fn errors_other_than_program_errors_keep_their_text() {
        let logs = logs(&[
            "Program 11111111111111111111111111111111 invoke [1]",
            "Program 11111111111111111111111111111111 failed: insufficient funds for instruction",
        ]);
        let insufficient_funds =
            TransactionError::InstructionError(0, InstructionError::InsufficientFunds);

        assert_eq!(
            describe_transaction_error(&insufficient_funds, &logs, no_idl),
            insufficient_funds.to_string()
        );
        assert_eq!(
            describe_transaction_error(&TransactionError::BlockhashNotFound, &logs, no_idl),
            TransactionError::BlockhashNotFound.to_string()
        );
    }

    #[test]
    fn without_the_failure_log_the_error_is_not_named() {
        let logs = logs(&[
            "Program TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA invoke [1]",
            "Log truncated",
        ]);

        assert_eq!(
            describe_transaction_error(&custom(1), &logs, no_idl),
            "Error processing Instruction 0: custom program error: 0x1"
        );
    }
}
