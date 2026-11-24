//! Ponto de entrada do executável `wintop.exe`.
//!
//! Toda a implementação e os testes unitários vivem em `src/lib.rs`, deixando o manifesto
//! `requireAdministrator` restrito ao binário `wintop` (`cargo test` roda sem elevação).

fn main() {
    wintop::run();
}
