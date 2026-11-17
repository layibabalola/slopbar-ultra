; SlopBar Ultra release installers only (scripts/publish-ultra-release.ps1 adds this file).

; SlopBar Ultra's watchdog (src-tauri/src/keep_running.rs): its task's name, before the user's SID.
; A test of these hooks may name its own task and markers folder by defining these first.
!ifndef KEEP_RUNNING_TASK
  !define KEEP_RUNNING_TASK "SlopBar Ultra keep running"
!endif

; The folder of the watchdog's markers (the app's machine-local data folder), into $R0.
!macro KEEP_RUNNING_MARKERS_TO_R0
  !ifdef KEEP_RUNNING_MARKERS
    StrCpy $R0 "${KEEP_RUNNING_MARKERS}"
  !else
    StrCpy $R0 "$LOCALAPPDATA\${BUNDLEID}\keep-running"
  !endif
!macroend

; Agents run slopbar-mcp.exe for their whole session, and a running image cannot be written, so a
; quiet update would skip it and leave the old build. Windows does let it be renamed: set it aside
; so the installer writes the new one. Running servers carry on from the old copy; the app deletes
; old copies once nothing runs them (src-tauri/src/mcp_update.rs).
; Every install also ends an uninstall's hold on the watchdog (its `uninstalling` marker).
!macro NSIS_HOOK_PREINSTALL
  Push $R0
  Push $R1
  Push $R2
  Push $R3
  Push $R4
  Push $R5
  Push $R6
  Push $R7
  IfFileExists "$INSTDIR\slopbar-mcp.exe" 0 slopbar_mcp_done
    ${GetTime} "" "L" $R0 $R1 $R2 $R3 $R4 $R5 $R6
    StrCpy $R7 0
    slopbar_mcp_next:
      IntOp $R7 $R7 + 1
      IfFileExists "$INSTDIR\slopbar-mcp.old-$R2$R1$R0-$R4$R5$R6-$R7.exe" slopbar_mcp_next
    ClearErrors
    Rename "$INSTDIR\slopbar-mcp.exe" "$INSTDIR\slopbar-mcp.old-$R2$R1$R0-$R4$R5$R6-$R7.exe"
    ClearErrors
  slopbar_mcp_done:
  !insertmacro KEEP_RUNNING_MARKERS_TO_R0
  Delete "$R0\uninstalling"
  ClearErrors
  Pop $R7
  Pop $R6
  Pop $R5
  Pop $R4
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
!macroend

; Before anything else is removed, and never on an update: the watchdog's `uninstalling` marker holds
; every check, and keeps a SlopBar that a check started meanwhile from registering the task again;
; then the task goes. Its name ends with the user's SID, read here from the uninstaller's own token
; (copied up to its end), and schtasks deletes it through nsExec, which shows no console window and
; waits at most 10 seconds. No slopbar.exe runs: a foreign one at this path would take any argument
; for a second launch.
!macro NSIS_HOOK_PREUNINSTALL
  ${If} $UpdateMode <> 1
    Push $R0
    Push $R1
    Push $R2
    Push $R3
    Push $R4
    Push $R5
    Push $R6
    Push $R7
    !insertmacro KEEP_RUNNING_MARKERS_TO_R0
    CreateDirectory "$R0"
    ClearErrors
    FileOpen $R1 "$R0\uninstalling" w
    ${IfNot} ${Errors}
      FileWrite $R1 "uninstalling"
      FileClose $R1
    ${EndIf}
    StrCpy $R4 ""
    System::Call 'advapi32::OpenProcessToken(p -1, i 8, *p .R1) i .R2'
    ${If} $R2 <> 0
      System::Call 'advapi32::GetTokenInformation(p R1, i 1, p 0, i 0, *i .R5)'
      System::Alloc $R5
      Pop $R3
      System::Call 'advapi32::GetTokenInformation(p R1, i 1, p R3, i R5, *i .R5) i .R2'
      ${If} $R2 <> 0
        System::Call '*$R3(p .R5)'
        System::Call 'advapi32::ConvertSidToStringSidW(p R5, *p .R6) i .R2'
        ${If} $R2 <> 0
          System::Call 'kernel32::lstrcpynW(w .R4, p R6, i ${NSIS_MAX_STRLEN})'
          System::Call 'kernel32::LocalFree(p R6)'
        ${EndIf}
      ${EndIf}
      System::Free $R3
      System::Call 'kernel32::CloseHandle(p R1)'
    ${EndIf}
    ${If} $R4 != ""
      nsExec::Exec /TIMEOUT=10000 '"$SYSDIR\schtasks.exe" /Delete /TN "${KEEP_RUNNING_TASK} $R4" /F'
      Pop $R7
    ${EndIf}
    ClearErrors
    Pop $R7
    Pop $R6
    Pop $R5
    Pop $R4
    Pop $R3
    Pop $R2
    Pop $R1
    Pop $R0
  ${EndIf}
!macroend

; The uninstaller removes slopbar-mcp.exe itself; copies set aside are not in its list. One an
; agent still runs is left, and so is the folder then. Updates never uninstall.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    Delete "$INSTDIR\slopbar-mcp.old-*.exe"
    RMDir "$INSTDIR"
  ${EndIf}
!macroend
