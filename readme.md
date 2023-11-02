# Rustls bench runner

This repository contains rustls' bench runner software, meant to be deployed to a bare-metal server
(though nothing prevents deploying to a VPS for testing). There are two main directories, each with
a readme providing more information:

- `ansible`: ansible scripts to set up the server and manage deployments.
- `ci-bench-runner`: a Rust binary that runs benchmarks and reports results based on GitHub activity.

## Configuration

#### GitHub app

The bench runner needs an associated [GitHub app](https://docs.github.com/en/apps) to receive event
notifications and react to them. The app should be private, to ensure it only gets installed on the
official rustls repository. If you are creating the GitHub app from scratch (e.g. for testing
purposes), ensure it requires the following _repository permissions_:

- Commit statuses: read and write.
- Contents: read-only.
- Issues: read and write.
- Metadata: read-only.
- Pull requests: read and write.

The app should also require subscriptions to the following events (don't forget to configure the
webhook's URL _and_ a secret to ensure event authenticity):

- Issue comment.
- Pull request.
- Pull request review.
- Push.

Make sure to set the GitHub webook URL for your application with the correct 
`/webhooks/github` route, e.g.:

- `https://my-domain.example.com/webhooks/github`

#### Server

To reduce noise, the server should be configured to disable dynamic frequency scaling (also known as
Turbo Boost) and hyper threading. This needs to be done at the BIOS / UEFI level.

## Features

The following features are supported:

- Run the benchmarks on every push to `main` and store the results.
- Calculate a per-benchmark significance threshold based on the result history for the `main` branch.
- Run the benchmarks on pull requests, comparing the results against the pull request's base branch.
  For security, comparison bench runs are only triggered in the following scenarios:
  - A PR is created or updated and the head branch lives in the rustls repository.
  - A maintainer leaves a GitHub review approving the PR.
  - A maintainer posts a comment to the PR including `@rustls-bench bench` as part of the body. This
    can be used as a fallback mechanism when the triggers mentioned above are not enough.
- Report comparison results in a comment to the relevant PR, reusing the same comment when new
  results are available.

Interesting ideas for later:

- Compile the benchmarks using different C toolchains, like GCC and LLVM (we are currently using the
  system's GCC).

## Nix Support

Since this repository requires both Ansible, and Rust, a [Nix flake] is provided
to offer a self-contained developer environment.

To use:
1. [Install Nix]
2. `nix develop`
3. You're all set! Use `cargo` and `ansible` as needed.

You may also install [Direnv] and `direnv allow` this directory to have the Nix
developer environment load/unload automatically when you enter/leave the
directory.

[Nix flake]: https://zero-to-nix.com/concepts/flakes
[Install Nix]: https://nixos.org/
[Direnv]: https://direnv.net/
