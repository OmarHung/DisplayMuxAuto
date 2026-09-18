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
;
; The registry says where the old copy is, but the per-user half of it is
; writable by anything running as the user, and this installer may run
; elevated. So its command line is never run: only an uninstall.exe found in
; an install location under FOLDER_A or FOLDER_B, where the matching kind of
; install puts it.
!macro RemoveLegacyInstall ROOT LEGACY_NAME FOLDER_A FOLDER_B
  ReadRegStr $R7 ${ROOT} "Software\Microsoft\Windows\CurrentVersion\Uninstall\${LEGACY_NAME}" "InstallLocation"
  ${If} $R7 != ""
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
    ; Resolved first, so a location written as FOLDER\..\elsewhere cannot pass
    ; the check below. Empty when the folder does not exist.
    GetFullPathName $R7 $R7
    StrCpy $R6 "${FOLDER_A}\"
    StrLen $R9 $R6
    StrCpy $R9 $R7 $R9
    ${If} $R9 != $R6
      StrCpy $R6 "${FOLDER_B}\"
      StrLen $R9 $R6
      StrCpy $R9 $R7 $R9
    ${EndIf}
    ${If} $R9 != $R6
      StrCpy $R7 ""
    ${EndIf}
    ; _?= is satisfied by exactly one directory: the one the uninstaller runs
    ; from. Checking for it is the same question, asked where a wrong answer
    ; costs nothing — rather than handing the path over and being told about it
    ; in a message box the user has to dismiss.
    ${If} $R7 != ""
    ${AndIf} ${FileExists} "$R7\uninstall.exe"
      ; The same call the installer makes when upgrading itself: /P for no
      ; prompts, and _?= to run the uninstaller where it stands. Without _?= it
      ; copies itself to a temp folder and returns at once, and the install
      ; below would race a deletion that is still running.
      ExecWait '"$R7\uninstall.exe" /P _?=$R7' $R8
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
  ; Elevated for the same account, this installer still sees that account's
  ; own registry and LocalAppData, which anything running as the user can write
  ; to. Running an uninstaller found there would run it as administrator, so a
  ; per-user copy is removed only by an installer that is not elevated either.
  System::Call 'shell32::IsUserAnAdmin() i .R9'
  ${If} $R9 == 0
    !insertmacro RemoveLegacyInstall HKCU "DisplayMux" "$LOCALAPPDATA" "$LOCALAPPDATA"
    !insertmacro RemoveLegacyInstall HKCU "DisplayMuxAuto" "$LOCALAPPDATA" "$LOCALAPPDATA"
  ${EndIf}
  !insertmacro RemoveLegacyInstall HKLM "DisplayMux" "$PROGRAMFILES64" "$PROGRAMFILES"
  !insertmacro RemoveLegacyInstall HKLM "DisplayMuxAuto" "$PROGRAMFILES64" "$PROGRAMFILES"
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
