; All hooks run elevated because installation is perMachine.
!macro NSIS_HOOK_PREINSTALL
  ; Warn before stopping the service or replacing files. Do not silently accept
  ; the risk in unattended installations.
  System::Call 'kernel32::GetFullPathNameW(w "$INSTDIR", i ${NSIS_MAX_STRLEN}, w .r0, p 0) i.r3'
  System::Call 'shlwapi::PathIsPrefixW(w "$PROGRAMFILES64", w "$0") i.r1'
  System::Call 'shlwapi::PathIsPrefixW(w "$PROGRAMFILES32", w "$0") i.r2'
  ${If} $1 == 0
  ${AndIf} $2 == 0
    MessageBox MB_YESNO|MB_ICONEXCLAMATION|MB_DEFBUTTON2 "您选择的安装目录为：$\r$\n$INSTDIR$\r$\n$\r$\n该目录不在系统 Program Files 中。VNT Remote 被控服务以 SYSTEM 最高权限运行；如果其他普通用户或程序可以修改安装目录或其父目录，就可能替换服务程序并获得系统权限。$\r$\n$\r$\n建议使用 Program Files，或由管理员保护自定义目录及其父目录。安装器不会修改自定义目录的访问权限。$\r$\n$\r$\n您是否理解并接受此风险，仍要继续安装？" /SD IDNO IDYES vnt_path_accepted
    SetErrorLevel 1
    Abort
  ${EndIf}
  vnt_path_accepted:
  IfFileExists "$INSTDIR\vnt-service.exe" 0 vnt_pre_done
    ExecWait '"$INSTDIR\vnt-service.exe" --prepare-upgrade' $0
    ${If} $0 != 0
      MessageBox MB_OK|MB_ICONSTOP "无法停止并保存 VNT Remote 被控服务状态。请关闭相关进程后重试安装。"
      Abort
    ${EndIf}
  vnt_pre_done:
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ExecWait '"$INSTDIR\vnt-service.exe" --install' $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONSTOP "Windows 被控服务安装失败。请检查安装目录是否可由 SYSTEM 账户访问，并检查日志：$INSTDIR\logs\vnt-installer.log。应用当前只能作为主控。"
    Abort
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  IfFileExists "$INSTDIR\vnt-service.exe" 0 vnt_un_done
    ClearErrors
    ${GetOptions} $CMDLINE "_?=" $R0
    ${IfNot} ${Errors}
      ExecWait '"$INSTDIR\vnt-service.exe" --uninstall-upgrade' $0
    ${ElseIf} $UpdateMode = 1
      ExecWait '"$INSTDIR\vnt-service.exe" --uninstall-upgrade' $0
    ${Else}
      ExecWait '"$INSTDIR\vnt-service.exe" --uninstall' $0
    ${EndIf}
    ${If} $0 != 0
      MessageBox MB_OK|MB_ICONSTOP "无法删除 VNT Remote 被控服务，已停止卸载。请确认管理员权限后重试。"
      Abort
    ${EndIf}
  vnt_un_done:
!macroend
