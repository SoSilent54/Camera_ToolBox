//! 通用显式导出服务；格式编码与持久化目标解耦。

use std::{
    io::{self, Write},
    sync::Arc,
};

use crate::{DirectoryRef, EntryName, FileRef, FileSystem, FileSystemError, FsControl};

/// 已完成编码、可写入任意文件系统目标的导出内容。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportArtifact {
    file_name: EntryName,
    bytes: Vec<u8>,
}

impl ExportArtifact {
    #[must_use]
    pub const fn new(file_name: EntryName, bytes: Vec<u8>) -> Self {
        Self { file_name, bytes }
    }

    #[must_use]
    pub const fn file_name(&self) -> &EntryName {
        &self.file_name
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Explorer 当前可写目录与其对应文件系统的稳定组合。
#[derive(Clone)]
pub struct ExportDestination {
    directory: DirectoryRef,
    file_system: Arc<dyn FileSystem>,
}

impl ExportDestination {
    /// # Errors
    ///
    /// 目录与文件系统不属于同一 source 时拒绝构造，避免跨挂载写入。
    pub fn new(
        directory: DirectoryRef,
        file_system: Arc<dyn FileSystem>,
    ) -> Result<Self, FileSystemError> {
        if &directory.source_id != file_system.source_id() {
            return Err(FileSystemError::SourceMismatch {
                expected: file_system.source_id().clone(),
                actual: directory.source_id,
            });
        }
        Ok(Self {
            directory,
            file_system,
        })
    }

    #[must_use]
    pub const fn directory(&self) -> &DirectoryRef {
        &self.directory
    }

    #[must_use]
    pub fn file_system(&self) -> &Arc<dyn FileSystem> {
        &self.file_system
    }
}

/// 一次成功导出后可用于刷新 Explorer 的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportReceipt {
    file: FileRef,
    bytes_written: u64,
}

impl ExportReceipt {
    #[must_use]
    pub const fn file(&self) -> &FileRef {
        &self.file
    }

    #[must_use]
    pub const fn bytes_written(&self) -> u64 {
        self.bytes_written
    }
}

/// 所有显式导出共用的 create-new 保存入口。
#[derive(Clone, Copy, Debug, Default)]
pub struct ExportService;

impl ExportService {
    /// # Errors
    ///
    /// 目标只读、source 不一致、文件已存在、取消或底层写入失败时返回错误。
    pub fn save_new(
        &self,
        destination: &ExportDestination,
        artifact: &ExportArtifact,
        control: &FsControl,
    ) -> Result<ExportReceipt, FileSystemError> {
        self.save_new_with(destination, artifact.file_name(), control, &mut |writer| {
            writer
                .write_all(artifact.bytes())
                .map_err(FileSystemError::io)
        })
    }

    /// 将任意流式 encoder 的输出保存为一个此前不存在的文件。
    ///
    /// # Errors
    ///
    /// 目标只读、source 不一致、文件已存在、producer 失败、取消或底层写入失败时返回错误。
    pub fn save_new_with(
        &self,
        destination: &ExportDestination,
        name: &EntryName,
        control: &FsControl,
        produce: &mut dyn FnMut(&mut dyn Write) -> Result<(), FileSystemError>,
    ) -> Result<ExportReceipt, FileSystemError> {
        control.checkpoint()?;
        if destination.directory.source_id != *destination.file_system.source_id() {
            return Err(FileSystemError::SourceMismatch {
                expected: destination.file_system.source_id().clone(),
                actual: destination.directory.source_id.clone(),
            });
        }
        if !destination.file_system.capabilities().write_new {
            return Err(FileSystemError::Unsupported);
        }
        let mut bytes_written = 0_u64;
        let mut counted_producer = |writer: &mut dyn Write| {
            let mut counting = CountingWriter {
                inner: writer,
                bytes_written: 0,
            };
            let result = produce(&mut counting);
            bytes_written = counting.bytes_written;
            result
        };
        let file = destination.file_system.write_new_atomic(
            &destination.directory,
            name,
            control,
            &mut counted_producer,
        )?;
        Ok(ExportReceipt {
            file,
            bytes_written,
        })
    }
}

struct CountingWriter<'a> {
    inner: &'a mut dyn Write,
    bytes_written: u64,
}

impl Write for CountingWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(bytes)?;
        let count = u64::try_from(written)
            .map_err(|_| io::Error::other("export write size cannot be represented as u64"))?;
        self.bytes_written = self
            .bytes_written
            .checked_add(count)
            .ok_or_else(|| io::Error::other("export byte count overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use parking_lot::Mutex;

    use super::*;
    use crate::{
        FileEntry, FileSourceId, FileSystemCapabilities, ListPage, ListPageRequest, ReadOutcome,
        ReadRequest, SourcePath,
    };

    struct RecordingFileSystem {
        source_id: FileSourceId,
        capabilities: FileSystemCapabilities,
        writes: Mutex<Vec<(DirectoryRef, EntryName, Vec<u8>)>>,
    }

    impl RecordingFileSystem {
        fn new(source_id: FileSourceId, capabilities: FileSystemCapabilities) -> Self {
            Self {
                source_id,
                capabilities,
                writes: Mutex::new(Vec::new()),
            }
        }
    }

    impl FileSystem for RecordingFileSystem {
        fn source_id(&self) -> &FileSourceId {
            &self.source_id
        }

        fn capabilities(&self) -> FileSystemCapabilities {
            self.capabilities
        }

        fn list(
            &self,
            _directory: &DirectoryRef,
            _page: ListPageRequest,
            _control: &FsControl,
        ) -> Result<ListPage, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn stat(
            &self,
            _reference: &FileRef,
            _control: &FsControl,
        ) -> Result<FileEntry, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn read(
            &self,
            _reference: &FileRef,
            _request: ReadRequest,
            _control: &FsControl,
            _consume: &mut dyn FnMut(&[u8]) -> Result<(), FileSystemError>,
        ) -> Result<ReadOutcome, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn write_new_atomic(
            &self,
            parent: &DirectoryRef,
            name: &EntryName,
            _control: &FsControl,
            produce: &mut dyn FnMut(&mut dyn Write) -> Result<(), FileSystemError>,
        ) -> Result<FileRef, FileSystemError> {
            let mut bytes = Vec::new();
            produce(&mut bytes)?;
            self.writes
                .lock()
                .push((parent.clone(), name.clone(), bytes));
            Ok(FileRef::new(self.source_id.clone(), parent.path.join(name)))
        }

        fn mkdir(
            &self,
            _parent: &DirectoryRef,
            _name: &EntryName,
            _control: &FsControl,
        ) -> Result<DirectoryRef, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn rename(
            &self,
            _reference: &FileRef,
            _new_name: &EntryName,
            _control: &FsControl,
        ) -> Result<FileRef, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn move_entry(
            &self,
            _reference: &FileRef,
            _destination: &DirectoryRef,
            _control: &FsControl,
        ) -> Result<FileRef, FileSystemError> {
            Err(FileSystemError::Unsupported)
        }

        fn delete(
            &self,
            _reference: &FileRef,
            _recursive: bool,
            _control: &FsControl,
        ) -> Result<(), FileSystemError> {
            Err(FileSystemError::Unsupported)
        }
    }

    #[test]
    fn export_service_routes_any_encoded_bytes_to_the_destination_port() {
        let source_id = FileSourceId::new("export-test").unwrap();
        let file_system = Arc::new(RecordingFileSystem::new(
            source_id.clone(),
            FileSystemCapabilities::READ_WRITE,
        ));
        let destination = ExportDestination::new(
            DirectoryRef::new(source_id, SourcePath::directory("results").unwrap()),
            file_system.clone(),
        )
        .unwrap();
        let artifact = ExportArtifact::new(
            EntryName::new("intrinsics.yaml").unwrap(),
            b"%YAML:1.0\n".to_vec(),
        );

        let receipt = ExportService
            .save_new(
                &destination,
                &artifact,
                &FsControl::with_timeout(Duration::from_secs(1)),
            )
            .unwrap();
        assert_eq!(
            receipt.file().path,
            SourcePath::new("results/intrinsics.yaml").unwrap()
        );
        assert_eq!(receipt.bytes_written(), 10);
        assert_eq!(
            *file_system.writes.lock(),
            vec![(
                destination.directory().clone(),
                artifact.file_name().clone(),
                artifact.bytes().to_vec(),
            )]
        );
    }

    #[test]
    fn export_service_streams_encoder_output_to_the_destination_port() {
        let source_id = FileSourceId::new("streaming-export-test").unwrap();
        let file_system = Arc::new(RecordingFileSystem::new(
            source_id.clone(),
            FileSystemCapabilities::READ_WRITE,
        ));
        let destination =
            ExportDestination::new(DirectoryRef::root(source_id), file_system.clone()).unwrap();
        let name = EntryName::new("result.bin").unwrap();

        let receipt = ExportService
            .save_new_with(
                &destination,
                &name,
                &FsControl::with_timeout(Duration::from_secs(1)),
                &mut |writer| {
                    writer.write_all(b"first-").map_err(FileSystemError::io)?;
                    writer.write_all(b"second").map_err(FileSystemError::io)
                },
            )
            .unwrap();

        assert_eq!(receipt.bytes_written(), 12);
        assert_eq!(file_system.writes.lock()[0].2, b"first-second".to_vec());
    }

    #[test]
    fn export_service_rejects_read_only_destination_before_writing() {
        let source_id = FileSourceId::new("read-only-export-test").unwrap();
        let file_system = Arc::new(RecordingFileSystem::new(
            source_id.clone(),
            FileSystemCapabilities::READ_ONLY,
        ));
        let destination =
            ExportDestination::new(DirectoryRef::root(source_id), file_system.clone()).unwrap();
        let artifact = ExportArtifact::new(EntryName::new("result.json").unwrap(), vec![1]);

        assert!(matches!(
            ExportService.save_new(
                &destination,
                &artifact,
                &FsControl::with_timeout(Duration::from_secs(1)),
            ),
            Err(FileSystemError::Unsupported)
        ));
        assert!(file_system.writes.lock().is_empty());
    }
}
