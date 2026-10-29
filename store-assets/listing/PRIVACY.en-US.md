# CorpMail Privacy Statement

Effective date: 2026-10-29

CorpMail ("this app") is a local-first business mail client. Our position on your privacy is the simplest one possible: **this app collects nothing, stores nothing off your device, and reports nothing about you.**

## We run no servers

- This app contains no server components operated by the developers.
- Your mail is delivered directly by the mail servers you configure (SMTP for sending / IMAP for receiving). The app is only a protocol client talking to those servers.
- Apart from your mail servers, the app sends data to no network endpoint.

## Data stays on your device

| Data | Where it lives | Notes |
|---|---|---|
| Message bodies, attachments, search index | Local database (SQLite) and attachment directory | Held in a disk directory you choose; can be moved, cleaned or deleted in-app at any time |
| Account configuration (servers, account name, display name) | Local config file | Contains only non-sensitive fields; never contains your password |
| Account password / authorization code | Windows Credential Manager / macOS Keychain | System-level secure storage; if the system credential service is unavailable, falls back to a locally encrypted file (key kept in the local database) — never plaintext |

## No telemetry, no analytics

- No analytics or crash-reporting SDKs are integrated.
- No anonymous device identifiers, no user profiling, no behavioral tracking.

## The only network traffic this app produces

1. **Sending (SMTP)**: delivering the messages you compose to your configured outbound server.
2. **Receiving (IMAP)**: fetching mail from, and syncing read state back to, your configured inbound server.

Both use only the server addresses and credentials you explicitly configure.

## Uninstalling and your data

- Uninstalling the app does not delete your mail data files (the data directory is independent of the program).
- You can see the data directory location in Settings → Storage, and delete it manually in File Explorer.

## Open source and verifiable

This app is open source under the MIT license:
https://github.com/jonathan197608/mailbox

## Contact

Issue tracker: https://github.com/jonathan197608/mailbox/issues
