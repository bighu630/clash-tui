# Agent Guidelines

## Development Policy

- **Testing**:
  - **Avoid Full Integration/Baseline Tests**: Do not run `cargo test --all` or full-scale integration tests during routine development as they are time-consuming and resource-intensive.
  - **Routine Testing**: Only run necessary unit tests (e.g., `cargo test --lib` or tests specific to the modified module) to ensure quick feedback loops.
  - **CI/Release**: Full-scale tests should only be triggered in CI pipelines or prior to formal releases.

## Git Workflow

- **Pre-push Hooks**: We use a `pre-push` hook to automate quality control. 
  - To enable, run: `git config core.hooksPath .githooks`
  - This hook automatically checks code formatting and runs baseline unit tests.
