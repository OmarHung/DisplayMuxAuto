; Removes earlier DisplayMux and DisplayMuxAuto installs before installing
; MuxSU over them.
;
; NSIS finds an earlier version by product name — its uninstall key is
; "...\Uninstall\${PRODUCTNAME}" — so once the app was renamed to
; DisplayMuxAuto, and now to MuxSU, its installer sees no differently named
; earlier version at all and installs alongside it instead of replacing it.
;
; Left there, the old copy is not merely an unused folder. Autostart is on by
; default, and its registry entry is keyed by product name too, so both copies
; launch at login and then contend for the same agent port and the same mDNS
; name — with no way for either to tell the user why.
;
; What follows is what a same-named upgrade would have done by itself.

; The old install may be per-user or per-machine, and this installer's own
; context says nothing about which one it was, so both are checked.
!macro RemoveLegacyInstall ROOT LEGACY_NAME
  ReadRegStr $R6 ${ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_NAME}" "UninstallString"
  ReadRegStr $R7 ${ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_NAME}" "InstallLocation"
  ${If} $R6 != ""
  ${AndIf} $R7 != ""
    ; The installer writes InstallLocation with its quotation marks included:
    ;   "C:\Users\...\DisplayMux"
    ; and _?= will not take a quoted path. Passed through as it is stored, the
    ; uninstaller cannot match the directory it is running from, says so in a
    ; message box, and does nothing — which is exactly what happened: a box to
    ; click past, an install that carried on, and the old copy still there.
    StrCpy $R9 $R7 1
    ${If} $R9 == "$\""
      StrLen $R9 $R7
      IntOp $R9 $R9 - 2
      StrCpy $R7 $R7 $R9 1
    ${EndIf}
    ; _?= is satisfied by exactly one directory: the one the uninstaller runs
    ; from. Checking for it is the same question, asked where a wrong answer
    ; costs nothing — rather than handing the path over and being told about it
    ; in a message box the user has to dismiss.
    ${If} ${FileExists} "$R7\uninstall.exe"
      ; The same call the installer makes when upgrading itself: /P for no
      ; prompts, and _?= to run the uninstaller where it stands. Without _?= it
      ; copies itself to a temp folder and returns at once, and the install
      ; below would race a deletion that is still running.
      StrCpy $R6 "$R6 /P"
      StrCpy $R6 "$R6 _?=$R7"
      ExecWait '$R6' $R8
      ; Only tidy up after an uninstall that actually ran. Deleting the
      ; uninstaller or the Programs and Features entry after a failure would
      ; leave the old files installed with nothing left to remove them by — and
      ; still starting at login.
      ${If} $R8 == 0
        ; Run in place, the uninstaller cannot delete itself or the folder
        ; holding it, so both are left to whoever called it.
        Delete "$R7\uninstall.exe"
        RMDir "$R7"
        DeleteRegKey ${ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_NAME}"
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  Push $R6
  Push $R7
  Push $R8
  Push $R9
  !insertmacro RemoveLegacyInstall HKCU "DisplayMux"
  !insertmacro RemoveLegacyInstall HKLM "DisplayMux"
  !insertmacro RemoveLegacyInstall HKCU "DisplayMuxAuto"
  !insertmacro RemoveLegacyInstall HKLM "DisplayMuxAuto"
  ; Written by the app at runtime rather than by the installer, so the
  ; uninstaller above leaves it. On its own it is enough to start the old copy
  ; at every login, whether or not the old files are still there.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DisplayMux"
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "DisplayMuxAuto"
  Pop $R9
  Pop $R8
  Pop $R7
  Pop $R6
!macroend
