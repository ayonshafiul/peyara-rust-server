; NSIS installer for Peyara Server (Windows).
; Built in CI from packaging/windows/stage/ containing:
;   peyara-egui.exe, icon.ico
; Invoke: makensis /DVERSION=1.0.1 ..\installer.nsi
; Output file lands next to this script; CI moves it to the repo root.

!ifndef VERSION
  !define VERSION "dev"
!endif

Name "Peyara Server ${VERSION}"
OutFile "..\Peyara-Server-${VERSION}-windows-x86_64-setup.exe"
InstallDir "$PROGRAMFILES\Peyara Server"
RequestExecutionLevel admin
Icon "icon.ico"

Page directory
Page instfiles

Section "Peyara Server"
  SetOutPath "$INSTDIR"
  File "peyara-egui.exe"
  File "icon.ico"
  CreateDirectory "$SMPROGRAMS\Peyara Server"
  CreateShortCut "$SMPROGRAMS\Peyara Server\Peyara Server.lnk" \
    "$INSTDIR\peyara-egui.exe" "" "$INSTDIR\icon.ico"
  WriteUninstaller "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Uninstall"
  Delete "$INSTDIR\peyara-egui.exe"
  Delete "$INSTDIR\icon.ico"
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$SMPROGRAMS\Peyara Server\Peyara Server.lnk"
  RMDir "$SMPROGRAMS\Peyara Server"
  RMDir "$INSTDIR"
SectionEnd
