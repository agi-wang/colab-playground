# W-ID account sync (work in progress)

The current command-line bridge is `tools/w_id_sync.py`. Run it with the Python interpreter that installed `google-colab-cli`, because that interpreter has the same keyring backend and `cryptography` dependency. Close Colab Playground before `pull` so it reloads `accounts.json` afterward.

```sh
COLAB_PLAYGROUND_SYNC_URL=https://auth.myfastools.com/colab-sync \
  /path/to/colab-cli-python tools/w_id_sync.py push

COLAB_PLAYGROUND_SYNC_URL=https://auth.myfastools.com/colab-sync \
  /path/to/colab-cli-python tools/w_id_sync.py pull
```

The first command opens W-ID's device authorization page. After W-ID login, enter the same six-digit PIN on each device. `push` uploads an AES-GCM encrypted snapshot of the saved Google account credentials; `pull` adds accounts that are missing locally without changing the active CLI account. The server stores a random key share and rate-limits PIN guesses. Because it receives the PIN during unlock, the self-hosted server is trusted with the vault; the design does not provide end-to-end secrecy from the server operator.

The desktop interface and the production HTTPS route are not connected yet. Do not distribute this as a finished sync release.
