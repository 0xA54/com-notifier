# COM Port Notifier
You probably don't want this, its windows only, built on a good portion of vibes (I don't really want to learn the Win32 APIs at the moment), and is an absolute mess at the moment, but it does beat opening device manager...

## Installation
*One day I'll add an --install flag*
1. Add `COM Notifier.lnk` to `C:\Users\Jed\AppData\Roaming\Microsoft\Windows\Start Menu\Programs`
2. Install `comport-register.reg`
3. Add `com-notifier.exe` to `C:\Program Files\COM Notifier\com-notifier.exe`


## TODOs
 - Add `--install`
 - Change the hard coded PuTTy implementation
 - Figure out baud rates, maybe with the input dialog in the toast
 - Expire toasts
