//! WinFSP callbacks.
//!
//! This module translates Windows kernel operations into the state, cache,
//! journal, and HTTP operations owned by the parent module.

use super::*;

impl FileSystemContext for RemoteWinFs {
    type FileContext = WinHandle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Result<FileSecurity> {
        let path = normalize_winfsp_path(file_name);
        let entry = self.metadata_for_path(&path)?;

        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: 0,
            attributes: file_attributes(&entry),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let path = normalize_winfsp_path(file_name);
        let entry = self.fill_file_info_for_path(&path, file_info.as_mut())?;
        let pending = self.writeback.get(&path);
        Ok(WinHandle::new_with_pending(path, entry, pending))
    }

    fn close(&self, context: Self::FileContext) {
        if let Some(pending) = &context.pending
            && !pending.is_committed()
            && !*context.delete_on_cleanup.lock().unwrap()
            && let Err(error) = self.writeback.enqueue(pending)
        {
            log::error!(
                "Could not queue {} for upload; its durable journal was retained: {error}",
                pending.path()
            );
        }
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: FILE_ACCESS_RIGHTS,
        _file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Result<Self::FileContext> {
        let path = normalize_winfsp_path(file_name);
        let api_path = remote_path::api(&path);
        let is_directory = create_options & FILE_DIRECTORY_FILE != 0;
        let (entry, pending) = if is_directory {
            let metadata = self
                .block_on(api::create_directory(&self.server_addr, api_path, 0o755))
                .map_err(|error| fsp_error_from_api(&error))?;
            (RemoteEntry::from_metadata(&metadata), None)
        } else {
            let pending = self
                .writeback
                .stage_new(&path, 0o644)
                .map_err(fsp_error_from_io)?;
            if let Some(bytes) = extra_buffer.filter(|bytes| !bytes.is_empty())
                && let Err(error) = pending.write_at(bytes, 0)
            {
                let _ = self.writeback.discard(&pending);
                return Err(fsp_error_from_io(error));
            }
            let entry = remote_entry_for_pending(&pending)?;
            (entry, Some(pending))
        };
        self.cache.insert_metadata(&path, &entry);
        self.cache.update_directory_for_path(&path, &entry);
        fill_file_info(file_info.as_mut(), &path, &entry);
        Ok(WinHandle::new_with_pending(path, entry, pending))
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, flags: u32) {
        let delete = *context.delete_on_cleanup.lock().unwrap()
            || FspCleanupFlags::FspCleanupDelete.is_flagged(flags);
        if delete {
            let path = context.path();
            let result = if let Some(pending) = &context.pending {
                if pending.is_committed() {
                    self.delete_path(&path, context.kind)
                } else {
                    self.writeback
                        .discard(pending)
                        .map_err(fsp_error_from_io)
                        .or_else(|_| {
                            self.materialize_pending(context)?;
                            self.delete_path(&path, context.kind)
                        })
                }
            } else {
                self.delete_path(&path, context.kind)
            };
            if let Err(error) = result {
                log::warn!("Failed to delete {path} during cleanup: {error:?}");
            } else {
                self.cache.invalidate_metadata_tree(&path);
                self.cache.remove_path(&path);
            }
        }
    }

    fn flush(&self, context: Option<&Self::FileContext>, file_info: &mut FileInfo) -> Result<()> {
        if let Some(context) = context {
            if let Some(pending) = &context.pending
                && !pending.is_committed()
            {
                self.writeback.enqueue(pending).map_err(fsp_error_from_io)?;
            }
            let path = context.path();
            fill_file_info(file_info, &path, &context.entry());
        } else {
            self.writeback.flush_all().map_err(fsp_error_from_io)?;
        }
        Ok(())
    }

    fn get_file_info(&self, context: &Self::FileContext, file_info: &mut FileInfo) -> Result<()> {
        let path = context.path();
        let entry = if let Some(pending) = &context.pending {
            if let Some(metadata) = pending.committed_metadata() {
                RemoteEntry::from_metadata(&metadata)
            } else {
                remote_entry_for_pending(pending)?
            }
        } else {
            self.metadata_for_path(&path)?
        };
        context.update_entry(entry.clone());
        fill_file_info(file_info, &path, &entry);
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        file_attributes: FILE_FLAGS_AND_ATTRIBUTES,
        replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        if context.kind != EntryKind::File {
            return Err(FspError::IO(ErrorKind::IsADirectory));
        }

        self.materialize_pending(context)?;
        let path = context.path();
        let current = self.metadata_for_path(&path)?;
        let mode = mode_after_overwrite(current.mode, file_attributes, replace_file_attributes);
        let metadata = self
            .block_on(api::overwrite_file(
                &self.server_addr,
                remote_path::api(&path),
                mode,
            ))
            .map_err(|error| fsp_error_from_api(&error))?;
        let entry = RemoteEntry::from_metadata(&metadata);
        context.update_entry(entry.clone());
        self.cache.insert_metadata(&path, &entry);
        self.cache.update_directory_for_path(&path, &entry);
        fill_file_info(file_info, &path, &entry);
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> Result<u32> {
        if context.kind != EntryKind::Directory {
            return Err(FspError::IO(ErrorKind::NotADirectory));
        }

        if marker.is_none() {
            let path = context.path();
            let entries = self.list_directory_cached(&path)?;
            let lock = context
                .dir_buffer
                .acquire(true, Some(entries.len() as u32 + 2))?;

            let current = context.entry();
            write_dir_entry(&lock, ".", &path, &current)?;
            let parent = remote_path::parent(&path);
            let parent_entry = self.metadata_for_path(parent)?;
            write_dir_entry(&lock, "..", parent, &parent_entry)?;

            for entry in entries {
                let child_path = remote_path::child(&path, &entry.name);
                let remote_entry = RemoteEntry::from_directory_entry(&entry);
                self.cache.insert_metadata(&child_path, &remote_entry);
                write_dir_entry(&lock, &entry.name, &child_path, &remote_entry)?;
            }
        }

        Ok(context.dir_buffer.read(marker, buffer))
    }

    fn rename(
        &self,
        context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Result<()> {
        let from = normalize_winfsp_path(file_name);
        let to = normalize_winfsp_path(new_file_name);

        self.materialize_pending(context)?;

        self.block_on(api::rename_file(
            &self.server_addr,
            remote_path::api(&from),
            remote_path::api(&to),
            replace_if_exists,
        ))
        .map_err(|error| fsp_error_from_api(&error))?;

        self.cache.clear();
        *context.path.lock().unwrap() = to.clone();
        self.cache.insert_metadata(&to, &context.entry());
        Ok(())
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        let path = context.path();
        let mtime = system_time_from_filetime(last_write_time);
        if mtime.is_some() {
            self.materialize_pending(context)?;
            let metadata = self
                .block_on(api::update_metadata(
                    &self.server_addr,
                    remote_path::api(&path),
                    None,
                    None,
                    None,
                    mtime,
                ))
                .map_err(|error| fsp_error_from_api(&error))?;
            let entry = RemoteEntry::from_metadata(&metadata);
            context.update_entry(entry.clone());
            self.cache.insert_metadata(&path, &entry);
            self.cache.update_directory_for_path(&path, &entry);
            fill_file_info(file_info, &path, &entry);
        } else {
            fill_file_info(file_info, &path, &context.entry());
        }
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> Result<()> {
        if delete_file {
            self.validate_delete(&context.path(), context.kind)?;
        }
        *context.delete_on_cleanup.lock().unwrap() = delete_file;
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Result<()> {
        if set_allocation_size {
            let path = context.path();
            fill_file_info(file_info, &path, &context.entry());
            return Ok(());
        }

        let path = context.path();
        if let Some(pending) = &context.pending
            && !pending.is_committed()
        {
            match pending.resize(new_size) {
                Ok(()) => {
                    let entry = remote_entry_for_pending(pending)?;
                    context.update_entry(entry.clone());
                    self.cache.insert_metadata(&path, &entry);
                    self.cache.update_directory_for_path(&path, &entry);
                    fill_file_info(file_info, &path, &entry);
                    return Ok(());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::AlreadyExists
                    ) =>
                {
                    self.materialize_pending(context)?;
                }
                Err(error) => return Err(fsp_error_from_io(error)),
            }
        }
        let metadata = self
            .block_on(api::resize_file(
                &self.server_addr,
                remote_path::api(&path),
                new_size,
            ))
            .map_err(|error| fsp_error_from_api(&error))?;
        let entry = RemoteEntry::from_metadata(&metadata);
        context.update_entry(entry.clone());
        self.cache.insert_metadata(&path, &entry);
        self.cache.update_directory_for_path(&path, &entry);
        fill_file_info(file_info, &path, &entry);
        Ok(())
    }

    fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
        if context.kind != EntryKind::File {
            return Err(FspError::IO(ErrorKind::IsADirectory));
        }

        if let Some(pending) = &context.pending
            && !pending.is_committed()
        {
            match pending.read_at(buffer, offset) {
                Ok(size) => return Ok(size as u32),
                Err(error)
                    if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::AlreadyExists) => {}
                Err(error) => return Err(fsp_error_from_io(error)),
            }
        }

        let path = context.path();
        let bytes = self
            .block_on(api::read_file(
                &self.server_addr,
                remote_path::api(&path),
                offset,
                buffer.len() as u32,
            ))
            .map_err(|error| fsp_error_from_api(&error))?;
        let bytes_to_copy = bytes.len().min(buffer.len());
        buffer[..bytes_to_copy].copy_from_slice(&bytes[..bytes_to_copy]);
        Ok(bytes_to_copy as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Result<u32> {
        if context.kind != EntryKind::File {
            return Err(FspError::IO(ErrorKind::IsADirectory));
        }

        let path = context.path();
        if let Some(pending) = &context.pending
            && !pending.is_committed()
        {
            let local_offset = if write_to_eof {
                pending.metadata().map_err(fsp_error_from_io)?.size
            } else {
                offset
            };
            match pending.write_at(buffer, local_offset) {
                Ok(()) => {
                    let entry = remote_entry_for_pending(pending)?;
                    context.update_entry(entry.clone());
                    self.cache.insert_metadata(&path, &entry);
                    self.cache.update_directory_for_path(&path, &entry);
                    fill_file_info(file_info, &path, &entry);
                    return Ok(buffer.len() as u32);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::AlreadyExists
                    ) =>
                {
                    self.materialize_pending(context)?;
                }
                Err(error) => return Err(fsp_error_from_io(error)),
            }
        }
        let offset = if write_to_eof {
            let metadata = self
                .block_on(api::get_metadata(
                    &self.server_addr,
                    remote_path::api(&path),
                ))
                .map_err(|error| fsp_error_from_api(&error))?;
            let entry = RemoteEntry::from_metadata(&metadata);
            let size = entry.size;
            context.update_entry(entry.clone());
            self.cache.insert_metadata(&path, &entry);
            size
        } else {
            offset
        };
        let metadata = self
            .block_on(api::write_file(
                &self.server_addr,
                remote_path::api(&path),
                buffer,
                offset,
            ))
            .map_err(|error| fsp_error_from_api(&error))?;
        let entry = RemoteEntry::from_metadata(&metadata);
        context.update_entry(entry.clone());
        self.cache.insert_metadata(&path, &entry);
        self.cache.update_directory_for_path(&path, &entry);
        fill_file_info(file_info, &path, &entry);
        Ok(buffer.len() as u32)
    }

    fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> Result<()> {
        out_volume_info.total_size = 1 << 40;
        out_volume_info.free_size = 1 << 39;
        out_volume_info.set_volume_label("remoteFS");
        Ok(())
    }
}
