# Contributing to send-rs

<!--toc:start-->

- [Contributing to send-rs](#contributing-to-send-rs)
  - [1. Before You Start](#1-before-you-start)
  - [2. Code of Conduct on Using AI](#2-code-of-conduct-on-using-ai)
  - [3. Contribution Workflow](#3-contribution-workflow)
    - [Small Changes (Typos, Minor Bugfixes, Small Cleanups)](#small-changes-typos-minor-bugfixes-small-cleanups)
    - [Big Changes (New Features, Architectural Changes, Refactors)](#big-changes-new-features-architectural-changes-refactors)
  - [4. Rust Quality & Testing Standards](#4-rust-quality-testing-standards)
  - [5. Commit Message Conventions](#5-commit-message-conventions)

<!--toc:end-->

First off, thank you for considering contributing to `send-rs`! Contributions
from the community help make this project better for everyone.

Below is a set of guidelines and best practices to follow when contributing to
this project.

## 1. Before You Start

To avoid duplicate work or conflicting updates, please complete the following
steps before writing any code:

1. **Check for Open Issues:**\
   Browse the
   [GitHub Issue Tracker](https://github.com/pedromarquetti/send-rs/issues) to
   see if someone else has already reported the issue or requested the feature.

2. **Search Codebase Tags:**\
   Check the codebase itself for pre-existing annotations. Maintainers and
   previous contributors often leave comments like `TODO`, `BUG`, or `INFO`
   directly in the source files.\
   _Tip:_ You can search for these tags locally using `grep`:
   ```bash
   grep -rnw 'src/' -e 'TODO' -e 'BUG' -e 'INFO'
   ```

---

## 2. Code of Conduct on Using AI

This project is built with the help of AI tooling, and we welcome AI-assisted
contributions, however, using AI requires extra attention to code quality:

- **Re-read and Verify Everything:** Always double- or triple-check any
  AI-generated code before committing. You are fully responsible for the
  functionality, safety, and logic of the code you submit.
- **Eliminate Redundancies:** AI tools often generate unnecessary boilerplate,
  redundant helper functions, or duplicated logic. Keep code clean, lean, and
  idiomatic.
- **Respect File Scope:** **Do not touch files outside the specific issue you
  are working on.** AI assistants often attempt broad refactors or format files
  in adjacent directories. Keep your diff strictly limited to the problem at
  hand.

---

## 3. Contribution Workflow

### Small Changes (Typos, Minor Bugfixes, Small Cleanups)

For minor updates, you can jump straight to creating a Pull Request:

1. **Fork** the repository to your own GitHub account.
2. **Clone** your fork locally:
   ```bash
   git clone https://github.com/YOUR_USERNAME/send-rs.git
   cd send-rs
   ```
3. Create a descriptive feature branch:
   ```bash
   git checkout -b fix/brief-description
   ```
4. Make your changes in your local environment.
5. Push your branch to GitHub and **create a Pull Request (PR)** against the
   `master` branch.

### Big Changes (New Features, Architectural Changes, Refactors)

1. **Create an Issue First:** Before writing code for major changes,
   [open a new issue](https://github.com/pedromarquetti/send-rs/issues/new)
   detailing what you plan to change and why.
2. **Discuss:** Wait for feedback and maintainer approval. Discussing design
   decisions early saves time for everyone.
3. **Follow the Workflow:** Once agreed upon, follow the standard Fork > Branch
   > Code > PR workflow described above.
4. **Reference the Issue:** Link your PR to the issue by including
   `Closes #<issue-number>` in the PR description.

> [!NOTE]
> Big changes without a discussion first will be rejected

---

## 4. Rust Quality & Testing Standards

To keep `send-rs` robust and maintainable, all PRs must adhere to standard Rust
tooling and checks:

1. **Format Code:**\
   Ensure your code matches the project styling by running:
   ```bash
   cargo fmt
   ```
2. **Run Linter:**\
   Check for warnings or bad practices using Clippy:
   ```bash
   cargo clippy -- -D warnings
   ```
3. **Pass Tests:**\
   Ensure existing tests pass and add new unit/integration tests for your
   changes where appropriate:
   ```bash
   cargo test
   ```

---

## 5. Commit Message Conventions

Clear commit messages help maintainers review changes quickly:

- Use the imperative mood (e.g., `add feature` instead of `added feature`).
- Keep the first line short (under 72 characters).
- Prefix commits logically where possible:
  - `feat:` for new features
  - `fix:` for bug fixes
  - `docs:` for documentation updates
  - `refactor:` for code restructuring without behavior changes
  - `test:` for adding or updating tests

---

Thank you again for helping improve **send-rs**!
