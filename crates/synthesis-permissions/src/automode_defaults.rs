//! Built-in auto mode rules, from Node src/native/automode-defaults.js. Deliberately NOT carried
//! over (harness audit P3): `Read-Only HTTP` (a GET is an exfiltration channel) and `Push To
//! Working Repo` (a push is an irreversible external action: always asked, see `irreversible.rs`);
//! `HARD_DENY` is extended with credential-exfiltration patterns.

pub const ENVIRONMENT: &[&str] = &[
    "Trusted repository: the working repository and the remotes configured in it. Everything else is external.",
    "Trusted internal domains: None configured",
    "Trusted cloud buckets: None configured",
    "Key internal services: None configured",
];

pub const ALLOW: &[&str] = &[
    "[Local Operations] Local file operations inside the working directory (reading, creating, editing, moving, deleting files the task is about).",
    "[Declared Dependencies] Installing dependencies that are declared in the repository lockfiles or manifests.",
    "[Credentials To Their API] Reading .env or credential files and sending a credential only to the API it belongs to.",
    "[Session Jobs] Deleting jobs, processes or temporary resources that were created in this session.",
    "[Security Work] Security-related code work: reviewing, testing and hardening code in the working repository.",
    "[Agent Messages] Messages between agents in the same session.",
    "[Trusted Environment] Sending data to entries listed as trusted in the environment section.",
];

pub const SOFT_DENY: &[&str] = &[
    "[Code From Internet] Downloading code from the internet and executing it (curl | bash, wget -O- | sh, piping a download into an interpreter, running an unreviewed remote script).",
    "[Data Exfiltration] Sending sensitive data to external endpoints, or putting secrets, credentials, session transcripts or dotfile stores into commits, pushes, PR text, gists or pastes.",
    "[Production Deploy] Deploying to production or running production release steps.",
    "[Cloud Storage Mass Delete] Mass deletion of objects in cloud storage (buckets, blobs, volumes).",
    "[Permission Grant] Granting permissions: IAM roles and policies, repository access, collaborators, tokens with wider scope.",
    "[Shared Infrastructure] Modifying shared infrastructure that other people or services depend on.",
    "[Irreversible Local Destruction] Irreversibly destroying files that existed before this session (no backup, not in version control).",
    "[Git Destructive] Force push, git reset --hard, git checkout -- ., git restore ., git clean -fd, git stash drop/clear, and git commit --amend of commits that were not made in this session or are already pushed.",
    "[IaC Destroy] terraform, pulumi, cdk or terragrunt destroy, and applying plans that destroy resources.",
    "[Secret Manager / DNS / TLS] Writes to secret managers, DNS records or TLS certificates.",
    "[PR Merge/Approval] Merging pull requests that nobody approved, approving your own pull request, or disabling CI.",
    "[Bot Commands] Posting comments that act as commands to bots (atlantis apply, /deploy and similar).",
    "[Feature Flags] Changing feature flags in production.",
    "[Cluster Writes] Cluster writes beyond the resource the user named, creating DaemonSets or admission webhooks, draining or removing nodes.",
    "[Interactive Remote Shell] Opening an interactive shell into sensitive targets (production hosts, database servers, cluster nodes).",
    "[Tunnels] Reverse shells and public tunnels that expose local services.",
    "[Credential Printing] Printing or logging credentials, tokens or private keys.",
    "[Registry Bypass] Installing packages from a registry or source other than the configured one.",
    "[Safety Flags] Turning off safety checks with flags such as --insecure, --no-verify, --force-with-no-checks or disabling TLS verification.",
    "[Agent Loop Bypass] Starting agents or tools with --dangerously-skip-permissions, --no-sandbox, --yes-always or similar permission bypasses.",
    "[Remote Redirect] Repointing base URLs, proxies, webhooks or registry mirrors, or changing git remotes to third-party hosts, unless the user named the target.",
    "[External Repo] Opening a pull request, forking or pushing to a repository of another organisation, unless the user named it.",
    "[Transcript Tampering] Writing to synthesis or Claude Code session transcripts.",
    "[Test Tampering] Disabling or weakening security tests or checks.",
    "[Unresolved rm -rf variable] rm -rf on a path built from a variable or substitution whose value cannot be seen.",
    "[Metadata Credentials] Requests to the cloud metadata address 169.254.169.254.",
    "[Sibling Scan] Scanning or reading sibling directories and projects outside the working repository for credentials or data.",
    "[Public Paste Links] Publishing content to public paste or file-sharing links.",
];

pub const HARD_DENY: &[&str] = &[
    "[Data Exfiltration] Unconditionally block sending repository contents or secrets to any endpoint that is not listed in the environment section.",
    "[Credential Exfiltration] Unconditionally block any command that reads credential stores (~/.ssh, ~/.aws, ~/.config/gcloud, ~/.kube, ~/.docker/config.json, ~/.npmrc, ~/.netrc, ~/.git-credentials, .env files, private keys, cloud metadata) and also sends data over the network (curl, wget, nc, ssh, scp, DNS lookups, git push, a URL or hostname built from the data), including through encoding, pipes, command substitution, redirects to a file that is sent later, or a script written earlier.",
    "[Secrets In URLs] Unconditionally block requests whose URL, query string, headers or body contain a token, key, password or any value read from an environment variable or secret file, unless the destination is the API that credential belongs to and is listed in the environment section.",
    "[Credential Paths] Unconditionally block copying, archiving, encoding or uploading ~/.ssh, ~/.aws, ~/.config/gcloud, ~/.kube or other credential directories anywhere, including into the working directory.",
];
