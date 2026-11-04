#[allow(unused_macros)]
#[macro_export]
macro_rules! green {
    ($($arg:tt)*) => (
        {
            use ansi_term::Colour;
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let colour = Colour::Green.bold();
                format!(
                    "{}",
                    colour.paint($($arg)*)
                )
            } else {
                format!(
                    "{}",
                    $($arg)*
                )
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! red {
    ($($arg:tt)*) => (
        {
            use ansi_term::Colour;
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let colour = Colour::Red.bold();
                format!(
                    "{}",
                    colour.paint($($arg)*)
                )
            } else {
                format!(
                    "{}",
                    $($arg)*
                )
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! yellow {
    ($($arg:tt)*) => (
        {
            use ansi_term::Colour;
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let colour = Colour::Yellow.bold();
                format!(
                    "{}",
                    colour.paint($($arg)*)
                )
            } else {
                format!(
                    "{}",
                    $($arg)*
                )
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! blue {
    ($($arg:tt)*) => (
        {
            use ansi_term::Colour;
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let colour = Colour::Cyan.bold();
                format!(
                    "{}",
                    colour.paint($($arg)*)
                )
            } else {
                format!(
                    "{}",
                    $($arg)*
                )
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! purple {
    ($($arg:tt)*) => (
        {
            use ansi_term::Colour;
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let colour = Colour::Purple.bold();
                format!(
                    "{}",
                    colour.paint($($arg)*)
                )
            } else {
                format!(
                    "{}",
                    $($arg)*
                )
            }
        }
    )
}

#[allow(unused_macros)]
#[macro_export]
macro_rules! black {
    ($($arg:tt)*) => (
        {
            use ansi_term::Colour;
            if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
                let colour = Colour::Fixed(244);
                format!(
                    "{}",
                    colour.paint($($arg)*)
                )
            } else {
                format!(
                    "{}",
                    $($arg)*
                )
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
