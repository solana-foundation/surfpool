// Color macros gate on BOTH `atty::is(Stream::Stdout)` AND
// `AgentMode::color != ColorChoice::Never`. Without the AgentMode check, ANSI
// escapes leak into the JSONL `msg` field under NO_DNA when stdout is a TTY

#[allow(unused_macros)]
#[macro_export]
macro_rules! green {
    ($($arg:tt)*) => (
        {
            use atty::Stream;
            use ansi_term::Colour;
            if atty::is(Stream::Stdout)
                && $crate::no_dna::AgentMode::from_env().color != $crate::no_dna::ColorChoice::Never
            {
                let colour = Colour::Green.bold();
                format!("{}", colour.paint($($arg)*))
            } else {
                format!("{}", $($arg)*)
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! red {
    ($($arg:tt)*) => (
        {
            use atty::Stream;
            use ansi_term::Colour;
            if atty::is(Stream::Stdout)
                && $crate::no_dna::AgentMode::from_env().color != $crate::no_dna::ColorChoice::Never
            {
                let colour = Colour::Red.bold();
                format!("{}", colour.paint($($arg)*))
            } else {
                format!("{}", $($arg)*)
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! yellow {
    ($($arg:tt)*) => (
        {
            use atty::Stream;
            use ansi_term::Colour;
            if atty::is(Stream::Stdout)
                && $crate::no_dna::AgentMode::from_env().color != $crate::no_dna::ColorChoice::Never
            {
                let colour = Colour::Yellow.bold();
                format!("{}", colour.paint($($arg)*))
            } else {
                format!("{}", $($arg)*)
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! blue {
    ($($arg:tt)*) => (
        {
            use atty::Stream;
            use ansi_term::Colour;
            if atty::is(Stream::Stdout)
                && $crate::no_dna::AgentMode::from_env().color != $crate::no_dna::ColorChoice::Never
            {
                let colour = Colour::Cyan.bold();
                format!("{}", colour.paint($($arg)*))
            } else {
                format!("{}", $($arg)*)
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! purple {
    ($($arg:tt)*) => (
        {
            use atty::Stream;
            use ansi_term::Colour;
            if atty::is(Stream::Stdout)
                && $crate::no_dna::AgentMode::from_env().color != $crate::no_dna::ColorChoice::Never
            {
                let colour = Colour::Purple.bold();
                format!("{}", colour.paint($($arg)*))
            } else {
                format!("{}", $($arg)*)
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! black {
    ($($arg:tt)*) => (
        {
            use atty::Stream;
            use ansi_term::Colour;
            if atty::is(Stream::Stdout)
                && $crate::no_dna::AgentMode::from_env().color != $crate::no_dna::ColorChoice::Never
            {
                let colour = Colour::Fixed(244);
                format!("{}", colour.paint($($arg)*))
            } else {
                format!("{}", $($arg)*)
            }
        }
    )
}

#[macro_export]
macro_rules! pluralize {
    ($value:expr, $word:expr) => {
        if $value > 1 {
            format!("{} {}s", $value, $word)
        } else {
            format!("{} {}", $value, $word)
        }
    };
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! format_err {
    ($($arg:tt)*) => (
        {
            format!("{} {}", red!("error:"), $($arg)*)
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! format_warn {
    ($($arg:tt)*) => (
        {
            format!("{} {}", yellow!("warn:"), $($arg)*)
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! format_note {
    ($($arg:tt)*) => (
        {
            format!("{} {}", blue!("note:"), $($arg)*)
        }
    )
}
