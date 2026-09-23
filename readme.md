# matrix-journal

This small program watches all inbound Matrix events of the following types, and writes them to standard output, leaving a journal of Matrix events:
  - message (including media)
  - reaction
  - sticker
  - redaction.

This output can be piped to other programs -- to trigger actions, for example.  It never sends events, except (optionally) read receipts.

## To Use

Setup is unfortunately pretty manual.  Create `~/.local/share/matrix-journal/session.json`:

    {
      "user": "@me:matrix.org",
      "password": "something",
      "homeserver": "https://matrix.org",
      "db_key": "long-random-string",
      "session": null,
      "sync_token": null,
    }

Then run `matrix-journal`.  It will log in as you and populate session details in the file.  Once this has happened, you can close the program and set the password to `null` in the file.

Matrix-journal will accept verification requests and blindly verify the peer device.

Options of interest:
  - `-r` to watch only one specific room.
  - `-x` to send read receipts to all received events.
  - `-j` to format the output as JSON for easier mechanical processing.
