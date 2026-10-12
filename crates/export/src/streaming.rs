use super::*;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ExportStatistics {
    pub message_count: u64,
    pub media_count: u64,
    pub missing_media_count: u64,
    /// HTML summary includes media from announcements hidden by simplify,
    /// matching the existing in-memory export's metadata.
    pub summary_media_count: u64,
}

/// A replayable, stable source. Conversations and messages must already be
/// filtered and ordered according to the package options. Implementations
/// should keep database reads in a single snapshot, and propagate callback
/// errors immediately. Only metadata needs to be retained between visits.
pub trait ExportSource {
    fn statistics(&self) -> ExportStatistics;
    fn visit_conversations(
        &self,
        visitor: &mut dyn FnMut(&str) -> Result<(), ExportError>,
    ) -> Result<(), ExportError>;
    fn visit_messages(
        &self,
        conversation_id: &str,
        visitor: &mut dyn FnMut(&MessageV1) -> Result<(), ExportError>,
    ) -> Result<(), ExportError>;
}

/// Stream a source using package metadata; `messages` and `media` are not read.
/// Publication, cancellation and escaping follow the ordinary export API.
pub fn export_streamed(
    package: &ExportPackage,
    source: &dyn ExportSource,
    target: &Path,
    cancelled: &AtomicBool,
) -> Result<ExportResult, ExportError> {
    if target.exists() {
        return Err(ExportError::TargetExists);
    }
    check_cancelled(cancelled)?;
    let parent = target.parent().ok_or(ExportError::UnsafePath)?;
    fs::create_dir_all(parent)?;
    let partial = partial_path(target)?;
    // Never remove or truncate another writer's partial file.
    let file = File::options()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let result = (|| {
        let statistics = source.statistics();
        let names = ExportNames::new(package);
        let mut writer = StreamWriter::new(file, package, statistics)?;
        let mut actual = ExportStatistics::default();
        source.visit_conversations(&mut |id| {
            check_cancelled(cancelled)?;
            let mut started = false;
            source.visit_messages(id, &mut |message| {
                check_cancelled(cancelled)?;
                if !started {
                    writer.begin_conversation(package, &names, id)?;
                    started = true;
                }
                writer.message(package, &names, message)?;
                actual.message_count += 1;
                actual.media_count += message.media.len() as u64;
                actual.missing_media_count += message
                    .media
                    .iter()
                    .filter(|item| item.integrity != MediaIntegrity::Verified)
                    .count() as u64;
                Ok(())
            })?;
            if started {
                writer.end_conversation(package)?;
            }
            Ok(())
        })?;
        if (
            actual.message_count,
            actual.media_count,
            actual.missing_media_count,
        ) != (
            statistics.message_count,
            statistics.media_count,
            statistics.missing_media_count,
        ) {
            return Err(io::Error::other("export source counts changed").into());
        }
        writer.finish(package)?;
        // Close the file before hashing and publishing (also required on Windows).
        drop(writer);
        check_cancelled(cancelled)?;
        let manifest_sha256 = hash_file(&partial)?;
        check_cancelled(cancelled)?;
        publish_without_overwrite(&partial, target)?;
        Ok(ExportResult {
            export_id: Uuid::new_v4(),
            target: target.to_path_buf(),
            message_count: actual.message_count,
            media_count: actual.media_count,
            missing_media_count: actual.missing_media_count,
            manifest_sha256,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

enum StreamWriter {
    Json(JsonStream),
    Csv(Box<csv::Writer<File>>),
    Html(BufWriter<File>),
    Md(BufWriter<File>),
}

impl StreamWriter {
    fn new(
        file: File,
        package: &ExportPackage,
        stats: ExportStatistics,
    ) -> Result<Self, ExportError> {
        Ok(match package.format {
            ExportFormat::Json => Self::Json(JsonStream::new(file, package.pretty)?),
            ExportFormat::Csv => {
                let mut file = file;
                file.write_all(&[0xEF, 0xBB, 0xBF])?;
                let mut output = csv::WriterBuilder::new().from_writer(file);
                csv_header(&mut output, package)?;
                Self::Csv(Box::new(output))
            }
            ExportFormat::Html => {
                let mut output = BufWriter::new(file);
                html_header(
                    &mut output,
                    package,
                    false,
                    stats.message_count,
                    stats.summary_media_count,
                )?;
                Self::Html(output)
            }
            ExportFormat::Md => {
                let mut output = BufWriter::new(file);
                markdown_header(&mut output, package, stats.message_count)?;
                Self::Md(output)
            }
        })
    }

    fn begin_conversation(
        &mut self,
        package: &ExportPackage,
        names: &ExportNames<'_>,
        id: &str,
    ) -> Result<(), ExportError> {
        match self {
            Self::Json(output) => output.begin_conversation(package, names, id),
            Self::Csv(_) => Ok(()),
            Self::Html(output) => html_conversation(output, package, names, id),
            Self::Md(output) => markdown_conversation(output, package, names, id),
        }
    }

    fn message(
        &mut self,
        package: &ExportPackage,
        names: &ExportNames<'_>,
        message: &MessageV1,
    ) -> Result<(), ExportError> {
        match self {
            Self::Json(output) => output.message(package, names, message),
            Self::Csv(output) => csv_message(output, package, names, message),
            Self::Html(output) => html_message(output, package, names, message),
            Self::Md(output) => markdown_message(output, package, names, message),
        }
    }

    fn end_conversation(&mut self, package: &ExportPackage) -> Result<(), ExportError> {
        match self {
            Self::Json(output) => output.end_conversation(package.pretty),
            Self::Html(output) => Ok(output.write_all(b"</section>")?),
            _ => Ok(()),
        }
    }

    fn finish(&mut self, package: &ExportPackage) -> Result<(), ExportError> {
        match self {
            Self::Json(output) => output.finish(package.pretty),
            Self::Csv(output) => Ok(output.flush()?),
            Self::Html(output) => {
                output.write_all(b"</main></body></html>")?;
                Ok(output.flush()?)
            }
            Self::Md(output) => Ok(output.flush()?),
        }
    }
}

pub(super) struct JsonStream {
    output: BufWriter<File>,
    has_conversation: bool,
    has_message: bool,
}

impl JsonStream {
    pub(super) fn new(file: File, pretty: bool) -> Result<Self, ExportError> {
        let mut output = BufWriter::new(file);
        output.write_all(if pretty {
            b"{\n  \"conversations\": ["
        } else {
            b"{\"conversations\":["
        })?;
        Ok(Self {
            output,
            has_conversation: false,
            has_message: false,
        })
    }

    pub(super) fn begin_conversation(
        &mut self,
        package: &ExportPackage,
        names: &ExportNames<'_>,
        id: &str,
    ) -> Result<(), ExportError> {
        if self.has_conversation {
            self.output.write_all(b",")?;
        }
        self.output.write_all(if package.pretty {
            b"\n    {\n      "
        } else {
            b"{"
        })?;
        let separator: &[u8] = if package.pretty { b": " } else { b":" };
        if package.simplify {
            self.output.write_all(b"\"conversation\"")?;
            self.output.write_all(separator)?;
            serde_json::to_writer(&mut self.output, names.conversation_id(id))?;
        } else {
            self.output.write_all(b"\"conversation_id\"")?;
            self.output.write_all(separator)?;
            serde_json::to_writer(&mut self.output, id)?;
            self.output.write_all(if package.pretty {
                b",\n      \"conversation_name\": "
            } else {
                b",\"conversation_name\":"
            })?;
            serde_json::to_writer(
                &mut self.output,
                names.conversation_name(id).unwrap_or("未命名会话"),
            )?;
        }
        self.output.write_all(if package.pretty {
            b",\n      \"messages\": ["
        } else {
            b",\"messages\":["
        })?;
        self.has_conversation = true;
        self.has_message = false;
        Ok(())
    }

    pub(super) fn message(
        &mut self,
        package: &ExportPackage,
        names: &ExportNames<'_>,
        message: &MessageV1,
    ) -> Result<(), ExportError> {
        if self.has_message {
            self.output.write_all(b",")?;
        }
        if package.pretty {
            self.output.write_all(b"\n        ")?;
        }
        if package.simplify {
            let value = SimplifiedMessage {
                sender: names.sender(message),
                content: message.body_text.as_deref(),
                media: message
                    .media
                    .iter()
                    .map(|item| item.original_name.as_deref().unwrap_or("未命名附件"))
                    .collect(),
                message_type: &message.message_type,
                sent_at: format_export_time(&message.sent_at),
            };
            self.value(&value, package.pretty)?;
        } else {
            self.value(&complete_message_value(message)?, package.pretty)?;
        }
        self.has_message = true;
        Ok(())
    }

    fn value(&mut self, value: &impl Serialize, pretty: bool) -> Result<(), ExportError> {
        if pretty {
            serde_json::to_writer_pretty(IndentedWriter(&mut self.output), value)?;
        } else {
            serde_json::to_writer(&mut self.output, value)?;
        }
        Ok(())
    }

    pub(super) fn end_conversation(&mut self, pretty: bool) -> Result<(), ExportError> {
        self.output
            .write_all(if pretty { b"\n      ]\n    }" } else { b"]}" })?;
        Ok(())
    }

    pub(super) fn finish(&mut self, pretty: bool) -> Result<(), ExportError> {
        if pretty && self.has_conversation {
            self.output.write_all(b"\n  ")?;
        }
        self.output
            .write_all(if pretty { b"]\n}\n" } else { b"]}" })?;
        Ok(self.output.flush()?)
    }
}

// Prefix nested pretty JSON lines without buffering an entire message/document.
struct IndentedWriter<'a>(&'a mut BufWriter<File>);
impl Write for IndentedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            self.0.write_all(part)?;
            if part.last() == Some(&b'\n') {
                self.0.write_all(b"        ")?;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixtureSource<'a> {
        groups: Vec<SelectedConversation<'a>>,
        statistics: ExportStatistics,
        after_message: Option<&'a dyn Fn() -> Result<(), ExportError>>,
    }
    impl<'a> FixtureSource<'a> {
        fn new(package: &'a ExportPackage) -> Self {
            let groups = selected_conversations(package);
            let mut statistics = ExportStatistics {
                summary_media_count: package.media.len() as u64,
                ..Default::default()
            };
            for message in groups.iter().flat_map(|group| &group.messages) {
                statistics.message_count += 1;
                statistics.media_count += message.media.len() as u64;
                statistics.missing_media_count += message
                    .media
                    .iter()
                    .filter(|item| item.integrity != MediaIntegrity::Verified)
                    .count() as u64;
            }
            Self {
                groups,
                statistics,
                after_message: None,
            }
        }
    }
    impl ExportSource for FixtureSource<'_> {
        fn statistics(&self) -> ExportStatistics {
            self.statistics
        }
        fn visit_conversations(
            &self,
            visitor: &mut dyn FnMut(&str) -> Result<(), ExportError>,
        ) -> Result<(), ExportError> {
            for group in &self.groups {
                visitor(group.conversation_id)?;
            }
            Ok(())
        }
        fn visit_messages(
            &self,
            id: &str,
            visitor: &mut dyn FnMut(&MessageV1) -> Result<(), ExportError>,
        ) -> Result<(), ExportError> {
            for message in &self
                .groups
                .iter()
                .find(|group| group.conversation_id == id)
                .unwrap()
                .messages
            {
                visitor(message)?;
                if let Some(hook) = self.after_message {
                    hook()?;
                }
            }
            Ok(())
        }
    }

    #[test]
    fn streamed_formats_match_existing_exports_with_all_options() {
        let directory = tempfile::tempdir().unwrap();
        for format in [
            ExportFormat::Json,
            ExportFormat::Csv,
            ExportFormat::Html,
            ExportFormat::Md,
        ] {
            for simplify in [false, true] {
                for pretty in [false, true] {
                    for ascending in [false, true] {
                        let mut package = crate::tests::fixture(format, simplify, pretty);
                        let mut other = package.messages[0].clone();
                        other.conversation_id = "second".into();
                        other.sent_at += chrono::TimeDelta::seconds(1);
                        package.messages.push(other);
                        package.conversation_order_ascending = ascending;
                        package.message_order_ascending = !ascending;
                        let reference = directory.path().join("reference");
                        let stream = directory.path().join("stream");
                        let original =
                            export_package(&package, &reference, &AtomicBool::new(false)).unwrap();
                        let result = export_streamed(
                            &package,
                            &FixtureSource::new(&package),
                            &stream,
                            &AtomicBool::new(false),
                        )
                        .unwrap();
                        assert_eq!(fs::read(&reference).unwrap(), fs::read(&stream).unwrap());
                        assert_eq!(result.message_count, original.message_count);
                        assert_eq!(result.manifest_sha256, original.manifest_sha256);
                        fs::remove_file(reference).unwrap();
                        fs::remove_file(stream).unwrap();
                    }
                }
            }
        }
    }

    #[test]
    fn streamed_failures_cancellation_and_publication_races_preserve_files() {
        let directory = tempfile::tempdir().unwrap();
        let package = crate::tests::fixture(ExportFormat::Json, false, false);
        let target = directory.path().join("result.json");
        let cancelled = AtomicBool::new(false);
        let cancel = || {
            cancelled.store(true, Ordering::Relaxed);
            Ok(())
        };
        let mut source = FixtureSource::new(&package);
        source.after_message = Some(&cancel);
        assert!(matches!(
            export_streamed(&package, &source, &target, &cancelled),
            Err(ExportError::Cancelled)
        ));
        assert!(!target.exists());
        assert!(!partial_path(&target).unwrap().exists());
        cancelled.store(false, Ordering::Relaxed);
        let fail = || Err(io::Error::other("synthetic source failure").into());
        source.after_message = Some(&fail);
        assert!(export_streamed(&package, &source, &target, &cancelled).is_err());
        assert!(!target.exists());
        assert!(!partial_path(&target).unwrap().exists());
        source.after_message = None;
        source.statistics.message_count += 1;
        assert!(export_streamed(&package, &source, &target, &cancelled).is_err());
        assert!(!target.exists());
        source.statistics.message_count -= 1;
        let competing_writer = || {
            fs::write(&target, b"existing output")?;
            Ok(())
        };
        source.after_message = Some(&competing_writer);
        assert!(matches!(
            export_streamed(&package, &source, &target, &cancelled),
            Err(ExportError::TargetExists)
        ));
        assert_eq!(fs::read(&target).unwrap(), b"existing output");
        assert!(!partial_path(&target).unwrap().exists());
        fs::remove_file(&target).unwrap();
        fs::write(partial_path(&target).unwrap(), b"active partial").unwrap();
        assert!(export_streamed(&package, &source, &target, &cancelled).is_err());
        assert_eq!(
            fs::read(partial_path(&target).unwrap()).unwrap(),
            b"active partial"
        );
    }
}
