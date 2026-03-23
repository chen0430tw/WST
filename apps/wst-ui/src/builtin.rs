// Builtin commands module
// Reserved for future expansion of builtin command system

#[allow(dead_code)]
pub struct BuiltinCommand {
    pub name: &'static str,
    pub description: &'static str,
    pub handler: fn(args: &[&str]) -> String,
}

#[allow(dead_code)]
pub const BUILTINS: &[BuiltinCommand] = &[
    BuiltinCommand {
        name: ":help",
        description: "Show available commands",
        handler: |_| "Available commands: :help, :status, :clear, :history, :backend, :exit".to_string(),
    },
];
