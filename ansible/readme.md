# Ansible scripts

This directory contains the necessary scripts for the bench server's setup and maintenance. They
allow deploying the bench server application, installing its dependencies, and exposing it to the
internet through HTTPS (including certificate renewal).

Important notes:

* Securing the server after setting up the operating system and creating users for server admins is not included.
* Performing a deploy _will_ affect benchmarking results (for example due to compiling Rust code for `ci-bench-runner`).
  Try to avoid deploying while benchmarking is in progress, or re-run benchmarks afterwards to avoid noise.

## Requirements

### Ansible configuration

The ansible scripts assume you are able to login as a sudoer user through passwordless SSH. You will
also need to install some Ansible collections:

1. `ansible-galaxy collection install community.general`

By default, the playbook will be run against the official rustls bench server, as configured in
[`inventory.ini`](inventory.ini).

### App configuration and secrets

The provided playbook requires access to a few secrets which are not part of the git repository. You
will need to:

1. Create a copy of `secrets.example.yml` and rename it to `secrets.yml`.
1. Replace the placeholder secret values by real ones.

Secrets will be injected in the Rust app configuration file, which can be found at
[`roles/bench-app-service/templates/config.json`](roles/bench-app-service/templates/config.json).
If you are deploying the  app for testing purposes, you will probably need to override some variables like
`github_app_id`, `github_repo_owner` and `github_repo_name` to match yours. See the [testing](#testing)
section for more information.

## General overview

The Rust application is deployed as a systemd unit and exposed to the internet through nginx as the
reverse proxy. Certificates are obtained from Let's Encrypt using certbot, which automatically takes
care of renewal. Rust itself is managed through rustup, because Debian's Rust version is otherwise
too old.

## Useful commands

- Setup everything from scratch and deploy: `ansible-playbook playbook.yml`
- Update Rust to the latest stable version: `ansible-playbook playbook.yml --tags rustup`
- Deploy a new version of the server: `ansible-playbook playbook.yml --tags deploy`
- Check the status of the application: `sudo systemctl status ci-bench-runner`

## Testing

In order to test changes to the Ansible playbooks, or the application code, it may be convenient to
deploy a modified version of this repository to a different server than the production instance. This
can help avoid disruptions and lets us catch bugs before they affect the production server.

### Setup

Before starting, you will need:

* A clean-slate server running Debian 12 (Bookworm).
* A domain name that is accessible from the broader internet, with A/AAAA records pointing to the IP
  address of the server. This is required for Certbot to obtain a HTTPS certificate from Let's Encrypt.
* A fork of the main [rustls](https://github.com/rustls/rustls) repository that you control.
* A GitHub application configured according to the [GitHub app](../readme.md#github-app) section of
  the main README. The application **must** be installed on your fork of the rustls repo, with the required
  permissions.
* If testing code changes to the `ci-bench-runner` app or Ansible playbooks, a fork of this repository that
  you control.

### Deployment

To deploy to your test server, copy `inventory.ini` to a new file `test-inventory.ini`, and update
the `ansible_host` IP address in `test-inventory.ini` to point to your clean-slate server.

You can then deploy using:
```bash
ansible-playbook \
  --inventory test-inventory.ini \
  --user root \
  --extra-vars 'hostname=rustls-bench.example.com' \
  --extra-vars 'github_repo_owner=example' \
  --extra_vars 'github_repo_name=rustls' \
  --extra-vars 'bench_app_repo=https://github.com/example/rustls-bench-app/' \
  --extra-vars 'bench_app_branch=example-branch' \
  --extra-vars 'bencher_api_token=jwt_header.jwt_payload.jwt_verify_signature' \
  --extra-vars 'bencher_project_id=rustls' \
  --extra-vars 'bencher_testbed_id=benchmarking-host' \
  playbook.yml
```

* The `hostname` variable should be configured to match the domain name you set up for your test server.
* The `github_repo_owner` and `github_repo_name` should be configured to match your fork of the main Rustls repo.
* The `bench_app_repo` and `bench_app_branch` should be configured to match your fork of this repository, and the
  branch you've pushed with your code changes (if applicable).
* The `bencher_api_token` should be configured to a Bencher API token for [the `rustls` project](https://bencher.dev/perf/rustls)
* The `bencher_project_id` should be configured to a Bencher Project UUID or slug. Optional value that defaults to `rustls`.
* The `bencher_testbed_id` should be configured to a Bencher Testbed UUID or slug. Optional value that defaults to `benchmarking-host`.