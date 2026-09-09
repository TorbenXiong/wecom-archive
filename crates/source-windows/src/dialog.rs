use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use archive_domain::DomainError;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FILEOPENDIALOGOPTIONS, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog,
    IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows::core::PCWSTR;

const CANCELLED_HRESULT: i32 = -2_147_023_673;

pub fn pick_folder(initial_folder: Option<PathBuf>) -> Result<Option<PathBuf>, DomainError> {
    std::thread::spawn(move || pick_folder_sta(initial_folder.as_deref()))
        .join()
        .map_err(|_| DomainError::Io("native folder picker thread failed".into()))?
}

fn pick_folder_sta(
    initial_folder: Option<&std::path::Path>,
) -> Result<Option<PathBuf>, DomainError> {
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(windows_error)?;
        let _com = ComApartment;
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(windows_error)?;
        let current = dialog.GetOptions().map_err(windows_error)?;
        dialog
            .SetOptions(FILEOPENDIALOGOPTIONS(
                current.0 | FOS_PICKFOLDERS.0 | FOS_FORCEFILESYSTEM.0 | FOS_PATHMUSTEXIST.0,
            ))
            .map_err(windows_error)?;
        if let Some(initial_folder) = initial_folder.filter(|path| path.is_dir()) {
            let mut wide = initial_folder.as_os_str().encode_wide().collect::<Vec<_>>();
            wide.push(0);
            if let Ok(item) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(wide.as_ptr()), None)
            {
                dialog.SetFolder(&item).map_err(windows_error)?;
            }
        }
        if let Err(error) = dialog.Show(None) {
            return if error.code().0 == CANCELLED_HRESULT {
                Ok(None)
            } else {
                Err(windows_error(error))
            };
        }
        let item = dialog.GetResult().map_err(windows_error)?;
        let display_name = item
            .GetDisplayName(SIGDN_FILESYSPATH)
            .map_err(windows_error)?;
        let result = display_name
            .to_string()
            .map(PathBuf::from)
            .map_err(|_| DomainError::Io("native dialog returned an invalid path".into()));
        CoTaskMemFree(Some(display_name.0.cast()));
        result.map(Some)
    }
}

struct ComApartment;

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

fn windows_error(error: windows::core::Error) -> DomainError {
    DomainError::Io(format!(
        "native dialog failed with status {:#010x}",
        error.code().0
    ))
}
