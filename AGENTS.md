## Project
mdk-recovery: A seed-only sweep tool of LDK on-chain funds for clients of [MoneyDevKit](https://moneydevkit.com/)
This project is still UNRELEASED so you do not need to worry about backwards compatibility.

## Commands
`just` lists all recipes. Use `just check` for quick validation (clippy, fmt, tests, nix)
as you work.

## Planning
- When planning a non-trivial change, you can use `git --no-pager log -n 3 -L <start>,<end>:<file>`
for more context about a particular section of code. Commit messages can carry important context in
this project so leverage that. Skip this for obvious fixes and additions.

## Guidelines
- **Pure core, thin effectful shell.** Separate logic from I/O. Build pure data structures that describe intent,
  then interpret them in a thin layer that performs effects. Test the pure core; the effectful shell should be too simple to fail.
- **Use integration tests to verify end-to-end behaviour** The unit tests cover the business logic but an integration test
  should still be added to verify end-to-end correctness. These often catch false assumptions of how LDK works.
- **Only make public what needs to be public** Watch out for and remove unnecessary `pub` modifiers.
- **Do not reference the plan in code comments.** The code comments should stand on their own as context for the code and not
  refer to plan details like commit numbers and planned followups. Genuine TODOs for deferred work like cleanups
  or refactors are okay when not being performed shortly.
- **Leverage the bkb-mcp tool** This gives you access to the Bitcoin Knowledge Base. Use this to discover and verify important details about the Bitcoin and Lightning protocols and how they are implemented in LDK.

## Commit Messages
The commit messaged must be focused on just the staged changes. Do not look at unstaged changes.
Use context from the conversation to help explain the changes.

Follow the seven basic rules on structure:
  - Separate subject from body with blank line
  - Limit subject to 50 characters (72 hard limit)
  - Capitalize subject line
  - No period at end of subject
  - Use imperative mood in subject (e.g., "Fix bug" not "Fixed bug" or "Fixes bug")
  - Wrap body at 72 characters
  - Body explains what and why, not how. The code diff explains how

Subject test: "If applied, this commit will [subject]" must make sense.

In addition:
- Avoid listing bullet points that are obvious from the code diff.
- Nobody should ever wonder why a particular change was made. That said, keep it
  concise and to the point.
- Do not reference plans.

## Style
- Rust 2024 edition
- Functional: iterators, combinators, folds over mutable loops
- Algebraic data types; make invalid states unrepresentable in the type system
- Static dispatch over trait objects (trait objects only for runtime polymorphism)
- Implement std traits (`FromStr`, `From`, `Display`) over ad-hoc methods
- Enum variants and match arms in alphabetical order
- Keep code comments brief. Leverage the commit messages for deeper explanations and context.

## Useful Information
- You can find LDK source code at ../ldk-node and ../rust-lightning

IMPORTANT: Check the README to see if it should be updated after completing a task
