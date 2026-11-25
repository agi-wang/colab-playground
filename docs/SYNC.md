# W-ID account sync

The top slot in the sidebar shows W-ID sign-in status. Select it to open sign-in and sync, where the W-ID email appears when available and the status can be refreshed. The Accounts page contains Google account details and switching. W-ID opens in your browser when sign-in is needed; approve the displayed device code. Then choose **Upload accounts** on the device that already has Google accounts, or **Restore accounts** on another device. Enter the same six-digit PIN on both devices. The app runs the bridge with the Colab CLI's Python interpreter so it shares the existing OS keyring.

Upload sends an AES-GCM encrypted snapshot of the saved Google account credentials. Restore adds accounts that are missing locally without changing the active CLI account; select an account in the app to switch to it. The server stores a random key share and rate-limits PIN guesses. Because it receives the PIN during unlock, the self-hosted server is trusted with the vault; the design does not provide end-to-end secrecy from the server operator.

The production HTTPS route is `https://auth.myfastools.com/colab-sync/`.
