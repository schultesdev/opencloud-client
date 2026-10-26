; NSIS installer for OpenCloud Sync (Phase 9).
;
; Built in CI with:
;   makensis /DVERSION=<x.y.z> /DSOURCE=<build-dir> /DOUTFILE=<setup.exe> installer.nsi
;
; VERSION and OUTFILE must be given on the command line; SOURCE defaults to
; the workspace target/release directory next to this script's repo layout.

!include "MUI2.nsh"
!include "FileFunc.nsh"

!ifndef VERSION
  !error "VERSION must be defined (/DVERSION=0.1.0)"
!endif
!ifndef OUTFILE
  !error "OUTFILE must be defined (/DOUTFILE=OpenCloud-Sync-Setup.exe)"
!endif
!ifndef SOURCE
  !define SOURCE "${__FILEDIR__}\..\..\target\release"
!endif

Name "OpenCloud Sync"
OutFile "${OUTFILE}"
Unicode true
InstallDir "$PROGRAMFILES64\OpenCloud Sync"
InstallDirRegKey HKLM "Software\OpenCloud Sync" "InstallDir"
RequestExecutionLevel admin

!define REG_UNINSTALL "Software\Microsoft\Windows\CurrentVersion\Uninstall\OpenCloud Sync"
!define APP_URL "https://www.schultes.dev"

!define MUI_ABORTWARNING
!define MUI_ICON "${__FILEDIR__}\..\icons\opencloud-sync.ico"
!define MUI_UNICON "${__FILEDIR__}\..\icons\opencloud-sync.ico"
!insertmacro MUI_PAGE_LICENSE "${__FILEDIR__}\..\..\LICENSE"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"
!insertmacro MUI_LANGUAGE "German"

Section "OpenCloud Sync (required)" SecCore
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "${SOURCE}\opencloud-sync.exe"
  File "${__FILEDIR__}\..\icons\opencloud-sync.ico"
  WriteRegStr HKLM "Software\OpenCloud Sync" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "${REG_UNINSTALL}" "DisplayName" "OpenCloud Sync"
  WriteRegStr HKLM "${REG_UNINSTALL}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${REG_UNINSTALL}" "DisplayIcon" "$INSTDIR\opencloud-sync.ico"
  WriteRegStr HKLM "${REG_UNINSTALL}" "Publisher" "Gregor Schulte"
  WriteRegStr HKLM "${REG_UNINSTALL}" "URLInfoAbout" "${APP_URL}"
  WriteRegStr HKLM "${REG_UNINSTALL}" "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
  WriteUninstaller "$INSTDIR\uninstall.exe"
  ; Estimate the installed size for the software list (in KiB).
  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKLM "${REG_UNINSTALL}" "EstimatedSize" "$0"
SectionEnd

Section "Start-menu shortcut" SecStartMenu
  CreateDirectory "$SMPROGRAMS\OpenCloud Sync"
  CreateShortcut "$SMPROGRAMS\OpenCloud Sync\OpenCloud Sync.lnk" \
    "$INSTDIR\opencloud-sync.exe" "" "$INSTDIR\opencloud-sync.ico"
  CreateShortcut "$SMPROGRAMS\OpenCloud Sync\Uninstall OpenCloud Sync.lnk" \
    "$INSTDIR\uninstall.exe"
SectionEnd

Section /o "Desktop shortcut" SecDesktop
  CreateShortcut "$DESKTOP\OpenCloud Sync.lnk" \
    "$INSTDIR\opencloud-sync.exe" "" "$INSTDIR\opencloud-sync.ico"
SectionEnd

LangString DescCore ${LANG_ENGLISH} "The program files for OpenCloud Sync."
LangString DescStartMenu ${LANG_ENGLISH} "Create shortcuts in the start menu."
LangString DescDesktop ${LANG_ENGLISH} "Create a shortcut on the desktop."
LangString DescCore ${LANG_GERMAN} "Die Programmdateien von OpenCloud Sync."
LangString DescStartMenu ${LANG_GERMAN} "Verknuepfungen im Startmenue anlegen."
LangString DescDesktop ${LANG_GERMAN} "Eine Verknuepfung auf dem Desktop anlegen."

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecCore} $(DescCore)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecStartMenu} $(DescStartMenu)
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} $(DescDesktop)
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  Delete "$SMPROGRAMS\OpenCloud Sync\OpenCloud Sync.lnk"
  Delete "$SMPROGRAMS\OpenCloud Sync\Uninstall OpenCloud Sync.lnk"
  RMDir "$SMPROGRAMS\OpenCloud Sync"
  Delete "$DESKTOP\OpenCloud Sync.lnk"
  Delete "$INSTDIR\opencloud-sync.exe"
  Delete "$INSTDIR\opencloud-sync.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  DeleteRegKey HKLM "Software\OpenCloud Sync"
  DeleteRegKey HKLM "${REG_UNINSTALL}"
SectionEnd
