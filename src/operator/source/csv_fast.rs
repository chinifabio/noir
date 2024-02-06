use std::fmt::Display;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

use csv::{Reader, ReaderBuilder, Terminator, Trim};

use crate::block::{BlockStructure, OperatorKind, OperatorStructure, Replication};
use crate::data_type::noir_data::NoirData;
use crate::data_type::noir_type::{NoirType, NoirTypeKind};
use crate::data_type::schema::Schema;
use crate::data_type::stream_item::StreamItem;
use crate::operator::source::Source;
use crate::operator::{Operator, StreamElement};
use crate::optimization::dsl::expressions::Expr;
use crate::scheduler::ExecutionMetadata;
use crate::Stream;

use super::{CsvOptions, LimitedReader};

/// Source that reads and parses a CSV file to NoirData rows.
///
/// The file is divided in chunks and is read concurrently by multiple replicas.
pub struct RowCsvSource {
    /// Path of the file.
    path: PathBuf,
    /// Reader used to parse the CSV file.
    csv_reader: Option<Reader<LimitedReader<BufReader<File>>>>,
    /// Options to customize the CSV parser.
    options: CsvOptions,
    /// Whether the reader has terminated its job.
    terminated: bool,
    replication: Replication,
    record: csv::StringRecord,
}

impl Display for RowCsvSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RowCsvSource<{}>", std::any::type_name::<NoirData>())
    }
}

impl RowCsvSource {
    /// Create a new source that reads and parse the lines of a CSV file.
    ///
    /// The file is partitioned into as many chunks as replicas, each replica has to have the
    /// **same** file in the same path. It is guaranteed that each line of the file is emitted by
    /// exactly one replica.
    ///
    /// After creating the source it's possible to customize its behaviour using one of the
    /// available methods. By default it is assumed that the delimiter is `,` and the CSV has
    /// headers.
    ///
    /// Each line will be deserialized into the type `Out`, so the structure of the CSV must be
    /// valid for that deserialization. The [`csv`](https://crates.io/crates/csv) crate is used for
    /// the parsing.
    ///
    /// **Note**: the file must be readable and its size must be available. This means that only
    /// regular files can be read.
    ///
    /// ## Example
    ///
    /// ```
    /// # use noir::{StreamEnvironment, EnvironmentConfig};
    /// # use noir::operator::source::CsvSource;
    /// # use serde::{Deserialize, Serialize};
    /// # let mut env = StreamEnvironment::new(EnvironmentConfig::local(1));
    /// #[derive(Clone, Deserialize, Serialize)]
    /// struct Thing {
    ///     what: String,
    ///     count: u64,
    /// }
    /// let source = CsvSource::<Thing>::new("/datasets/huge.csv");
    /// let s = env.stream(source);
    /// ```
    pub fn new<P: Into<PathBuf>>(path: P) -> Self {
        Self {
            path: path.into(),
            csv_reader: None,
            options: Default::default(),
            terminated: false,
            replication: Replication::Unlimited,
            record: csv::StringRecord::new(),
        }
    }

    /// The comment character to use when parsing CSV.
    ///
    /// If the start of a record begins with the byte given here, then that line is ignored by the
    /// CSV parser.
    ///
    /// This is disabled by default.
    pub fn comment(mut self, comment: Option<u8>) -> Self {
        self.options.comment = comment;
        self
    }

    /// The field delimiter to use when parsing CSV.
    ///
    /// The default is `,`.
    pub fn delimiter(mut self, delimiter: u8) -> Self {
        self.options.delimiter = delimiter;
        self
    }

    /// Enable double quote escapes.
    ///
    /// This is enabled by default, but it may be disabled. When disabled, doubled quotes are not
    /// interpreted as escapes.
    pub fn double_quote(mut self, double_quote: bool) -> Self {
        self.options.double_quote = double_quote;
        self
    }

    /// The escape character to use when parsing CSV.
    ///
    /// In some variants of CSV, quotes are escaped using a special escape character like `\`
    /// (instead of escaping quotes by doubling them).
    ///
    /// By default, recognizing these idiosyncratic escapes is disabled.
    pub fn escape(mut self, escape: Option<u8>) -> Self {
        self.options.escape = escape;
        self
    }

    /// Whether the number of fields in records is allowed to change or not.
    ///
    /// When disabled (which is the default), parsing CSV data will return an error if a record is
    /// found with a number of fields different from the number of fields in a previous record.
    ///
    /// When enabled, this error checking is turned off.
    pub fn flexible(mut self, flexible: bool) -> Self {
        self.options.flexible = flexible;
        self
    }

    /// The quote character to use when parsing CSV.
    ///
    /// The default is `"`.
    pub fn quote(mut self, quote: u8) -> Self {
        self.options.quote = quote;
        self
    }

    /// Enable or disable quoting.
    ///
    /// This is enabled by default, but it may be disabled. When disabled, quotes are not treated
    /// specially.
    pub fn quoting(mut self, quoting: bool) -> Self {
        self.options.quoting = quoting;
        self
    }

    /// The record terminator to use when parsing CSV.
    ///
    /// A record terminator can be any single byte. The default is a special value,
    /// `Terminator::CRLF`, which treats any occurrence of `\r`, `\n` or `\r\n` as a single record
    /// terminator.
    pub fn terminator(mut self, terminator: Terminator) -> Self {
        self.options.terminator = terminator;
        self
    }

    /// Whether fields are trimmed of leading and trailing whitespace or not.
    ///
    /// By default, no trimming is performed. This method permits one to override that behavior and
    /// choose one of the following options:
    ///
    /// 1. `Trim::Headers` trims only header values.
    /// 2. `Trim::Fields` trims only non-header or "field" values.
    /// 3. `Trim::All` trims both header and non-header values.
    ///
    /// A value is only interpreted as a header value if this CSV reader is configured to read a
    /// header record (which is the default).
    ///
    /// When reading string records, characters meeting the definition of Unicode whitespace are
    /// trimmed. When reading byte records, characters meeting the definition of ASCII whitespace
    /// are trimmed. ASCII whitespace characters correspond to the set `[\t\n\v\f\r ]`.
    pub fn trim(mut self, trim: Trim) -> Self {
        self.options.trim = trim;
        self
    }

    /// Whether to treat the first row as a special header row.
    ///
    /// By default, the first row is treated as a special header row, which means the header is
    /// never returned by any of the record reading methods or iterators. When this is disabled
    /// (`yes` set to `false`), the first row is not treated specially.
    ///
    /// Note that the `headers` and `byte_headers` methods are unaffected by whether this is set.
    /// Those methods always return the first record.
    pub fn has_headers(mut self, has_headers: bool) -> Self {
        self.options.has_headers = has_headers;
        self
    }

    pub fn replication(mut self, replication: Replication) -> Self {
        self.replication = replication;
        self
    }

    pub(crate) fn filter_at_source(mut self, predicate: Option<Expr>) -> Self {
        self.options.filter_at_source = predicate;
        self
    }

    pub(crate) fn project_at_source(mut self, projections: Option<Vec<usize>>) -> Self {
        self.options.projections_at_source = projections;
        self
    }

    pub(crate) fn with_schema(mut self, schema: Option<Schema>) -> Self {
        self.options.schema = schema;
        self
    }
}

impl Source for RowCsvSource {
    fn replication(&self) -> Replication {
        self.replication
    }
}

impl Operator for RowCsvSource {
    type Out = StreamItem;

    fn setup(&mut self, metadata: &mut ExecutionMetadata) {
        info!("CsvSource: setup from {}", metadata.global_id);
        let global_id = metadata.global_id;
        let instances = metadata.replicas.len();

        let file = File::options()
            .read(true)
            .write(false)
            .open(&self.path)
            .unwrap_or_else(|err| {
                panic!(
                    "CsvSource: error while opening file {:?}: {:?}",
                    self.path, err
                )
            });

        let file_size = file.metadata().unwrap().len();

        let mut buf_reader = BufReader::new(file);

        let last_byte_terminator = match self.options.terminator {
            Terminator::CRLF => b'\n',
            Terminator::Any(terminator) => terminator,
            _ => unreachable!(),
        };

        // Handle the header
        let mut header = Vec::new();
        let header_size = if self.options.has_headers {
            buf_reader
                .read_until(last_byte_terminator, &mut header)
                .expect("Error while reading CSV header") as u64
        } else {
            0
        };

        // Calculate start and end offset of this replica
        let body_size = file_size - header_size;
        let range_size = body_size / instances as u64;
        let mut start = header_size + range_size * global_id;
        let mut end = if global_id as usize == instances - 1 {
            file_size
        } else {
            start + range_size
        };

        // Align start byte
        if global_id != 0 {
            // Seek reader to the first byte to be read
            buf_reader
                .seek(SeekFrom::Start(start))
                .expect("Error while seeking BufReader to start");
            // discard first line
            let mut buf = Vec::new();
            start += buf_reader
                .read_until(last_byte_terminator, &mut buf)
                .expect("Error while reading first line from file") as u64;
        }

        // Align end byte
        if global_id as usize != instances - 1 {
            // Seek reader to the last byte to be read
            buf_reader
                .seek(SeekFrom::Start(end))
                .expect("Error while seeking BufReader to end");
            // get to the end of the line
            let mut buf = Vec::new();
            end += buf_reader
                .read_until(last_byte_terminator, &mut buf)
                .expect("Error while reading last line from file") as u64;
        }

        // Rewind BufReader to the start
        buf_reader
            .seek(SeekFrom::Start(start))
            .expect("Error while rewinding BufReader");

        // Limit the number of bytes to be read
        let limited_reader = LimitedReader::new(buf_reader, (end - start) as usize);

        // Create csv::Reader
        let mut csv_reader = ReaderBuilder::new()
            .comment(self.options.comment)
            .delimiter(self.options.delimiter)
            .double_quote(self.options.double_quote)
            .escape(self.options.escape)
            .flexible(self.options.flexible)
            .quote(self.options.quote)
            .quoting(self.options.quoting)
            .terminator(self.options.terminator)
            .trim(self.options.trim)
            .has_headers(self.options.has_headers)
            .from_reader(limited_reader);

        if self.options.has_headers {
            // set the headers of the CSV file
            csv_reader.set_byte_headers(
                Reader::from_reader(header.as_slice())
                    .byte_headers()
                    .unwrap()
                    .to_owned(),
            );
        }

        self.csv_reader = Some(csv_reader);
    }

    fn next(&mut self) -> StreamElement<StreamItem> {
        loop {
            if self.terminated {
                return StreamElement::Terminate;
            }
            let csv_reader = self
                .csv_reader
                .as_mut()
                .expect("CsvSource was not initialized");

            let data = match csv_reader.read_record(&mut self.record) {
                Ok(true) => match &self.options.schema {
                    Some(schema) => self.handle_record_with_schema(schema),
                    None => self.handle_record_without_schema(),
                },
                Ok(false) => {
                    self.terminated = true;
                    None
                }
                Err(e) => panic!("Error while reading CSV file: {:?}", e),
            };
            match (data, &self.options.filter_at_source) {
                (Some(item), Some(filter)) => {
                    let item = StreamItem::from(item);
                    if filter.evaluate(&item).into() {
                        return StreamElement::Item(item);
                    }
                }
                (Some(item), None) => return StreamElement::Item(StreamItem::from(item)),
                _ => return StreamElement::FlushAndRestart,
            }
        }
    }

    fn structure(&self) -> BlockStructure {
        let mut operator = OperatorStructure::new::<NoirData, _>("RowCsvSource");
        operator.kind = OperatorKind::Source;
        BlockStructure::default().add_operator(operator)
    }
}

impl RowCsvSource {
    fn handle_record_without_schema(&self) -> Option<NoirData> {
        if self.record.len() == 1 {
            let field = self.record.get(0).unwrap();
            if field.is_empty() {
                return Some(NoirData::NoirType(NoirType::None()));
            } else if let Ok(int_value) = field.parse::<i32>() {
                return Some(NoirData::NoirType(NoirType::Int32(int_value)));
            } else if let Ok(float_value) = field.parse::<f32>() {
                return Some(NoirData::NoirType(NoirType::Float32(float_value)));
            } else {
                return Some(NoirData::NoirType(NoirType::None()));
            }
        }

        if let Some(projections) = &self.options.projections_at_source {
            let mut data: Vec<NoirType> = Vec::with_capacity(self.record.len() - projections.len());
            for index in projections {
                let field = self.record.get(*index).unwrap();
                if field.is_empty() {
                    data.push(NoirType::None());
                } else if let Ok(int_value) = field.parse::<i32>() {
                    data.push(NoirType::Int32(int_value));
                } else if let Ok(float_value) = field.parse::<f32>() {
                    data.push(NoirType::Float32(float_value));
                } else {
                    data.push(NoirType::None());
                }
            }
            return Some(NoirData::Row(data));
        }

        let mut data: Vec<NoirType> = Vec::with_capacity(self.record.len());
        for field in self.record.iter() {
            if field.is_empty() {
                data.push(NoirType::None());
            } else if let Ok(int_value) = field.parse::<i32>() {
                data.push(NoirType::Int32(int_value));
            } else if let Ok(float_value) = field.parse::<f32>() {
                data.push(NoirType::Float32(float_value));
            } else {
                data.push(NoirType::None());
            }
        }
        Some(NoirData::Row(data))
    }

    fn handle_record_with_schema(&self, schema: &Schema) -> Option<NoirData> {
        if self.record.len() != schema.columns.len() {
            panic!(
                "CsvSource: record length ({}) does not match schema length ({})",
                self.record.len(),
                schema.columns.len()
            );
        }

        if self.record.len() == 1 {
            let field = self.record.get(0).unwrap();
            if field.is_empty() {
                return Some(NoirData::NoirType(NoirType::None()));
            }
            match schema.columns[0] {
                NoirTypeKind::Int32 => {
                    return field
                        .parse::<i32>()
                        .map(|v| NoirData::NoirType(NoirType::Int32(v)))
                        .ok()
                }
                NoirTypeKind::Float32 => {
                    return field
                        .parse::<f32>()
                        .map(|v| NoirData::NoirType(NoirType::Float32(v)))
                        .ok()
                }
                _ => return Some(NoirData::NoirType(NoirType::None())),
            }
        }

        if let Some(projections) = &self.options.projections_at_source {
            let mut data = Vec::with_capacity(self.record.len() - schema.columns.len());
            for index in projections {
                match schema.columns[*index] {
                    NoirTypeKind::Int32 => {
                        let f = self.record.get(*index).unwrap();
                        if f.is_empty() {
                            data.push(NoirType::None());
                        } else if let Ok(int_value) = f.parse::<i32>() {
                            data.push(NoirType::Int32(int_value));
                        } else {
                            data.push(NoirType::None());
                        }
                    }
                    NoirTypeKind::Float32 => {
                        let f = self.record.get(*index).unwrap();
                        if f.is_empty() {
                            data.push(NoirType::None());
                        } else if let Ok(float_value) = f.parse::<f32>() {
                            data.push(NoirType::Float32(float_value));
                        } else {
                            data.push(NoirType::None());
                        }
                    }
                    _ => return Some(NoirData::NoirType(NoirType::None())),
                }
            }
            return Some(NoirData::Row(data));
        }

        let mut data: Vec<NoirType> = Vec::with_capacity(self.record.len());
        for (field, column) in self.record.iter().zip(schema.columns.iter()) {
            match column {
                NoirTypeKind::Int32 => {
                    if field.is_empty() {
                        data.push(NoirType::None());
                    } else if let Ok(int_value) = field.parse::<i32>() {
                        data.push(NoirType::Int32(int_value));
                    } else {
                        data.push(NoirType::None());
                    }
                }
                NoirTypeKind::Float32 => {
                    if field.is_empty() {
                        data.push(NoirType::None());
                    } else if let Ok(float_value) = field.parse::<f32>() {
                        data.push(NoirType::Float32(float_value));
                    } else {
                        data.push(NoirType::None());
                    }
                }
                _ => return Some(NoirData::NoirType(NoirType::None())),
            }
        }
        Some(NoirData::Row(data))
    }
}

impl Clone for RowCsvSource {
    fn clone(&self) -> Self {
        assert!(
            self.csv_reader.is_none(),
            "CsvSource must be cloned before calling setup"
        );
        Self {
            path: self.path.clone(),
            csv_reader: None,
            options: self.options.clone(),
            terminated: false,
            replication: self.replication,
            record: csv::StringRecord::new(),
        }
    }
}

impl crate::StreamEnvironment {
    /// Convenience method, creates a `CsvSource` and makes a stream using `StreamEnvironment::stream`
    pub fn stream_csv_noirdata(
        &mut self,
        path: impl Into<PathBuf>,
    ) -> Stream<impl Operator<Out = StreamItem>> {
        let source = RowCsvSource::new(path);
        self.stream(source)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use itertools::Itertools;
    use tempfile::NamedTempFile;

    use crate::config::EnvironmentConfig;
    use crate::data_type::noir_data::NoirData;
    use crate::data_type::noir_type::NoirType;
    use crate::environment::StreamEnvironment;
    use crate::operator::source::csv_fast::RowCsvSource;

    #[test]
    fn csv_noir_data() {
        for num_records in 0..100 {
            for terminator in &["\n", "\r\n"] {
                let file = NamedTempFile::new().unwrap();
                for i in 0..num_records {
                    write!(file.as_file(), "{},{},{}", i, i as f32 + 0.5, terminator).unwrap();
                }

                let mut env = StreamEnvironment::new(EnvironmentConfig::local(4));
                let source = RowCsvSource::new(file.path()).has_headers(false);
                let res = env.stream(source).map(NoirData::from).collect_vec();
                env.execute_blocking();

                let res = res.get().unwrap().into_iter().sorted().collect_vec();
                assert_eq!(
                    res,
                    (0..num_records)
                        .map(|x| NoirData::Row(vec![
                            NoirType::from(x),
                            NoirType::from(x as f32 + 0.5),
                            NoirType::None()
                        ]))
                        .collect_vec()
                );
            }
        }
    }

    #[test]
    fn csv_noir_data_header() {
        for num_records in 0..100 {
            for terminator in &["\n", "\r\n"] {
                let file = NamedTempFile::new().unwrap();
                write!(file.as_file(), "a,b,c{terminator}").unwrap();
                for i in 0..num_records {
                    write!(file.as_file(), "{},{},{}", i, i as f32 + 0.5, terminator).unwrap();
                }

                let mut env = StreamEnvironment::new(EnvironmentConfig::local(4));
                let source = RowCsvSource::new(file.path());
                let res = env.stream(source).map(NoirData::from).collect_vec();
                env.execute_blocking();

                let res = res.get().unwrap().into_iter().sorted().collect_vec();
                assert_eq!(
                    res,
                    (0..num_records)
                        .map(|x| NoirData::Row(vec![
                            NoirType::from(x),
                            NoirType::from(x as f32 + 0.5),
                            NoirType::None()
                        ]))
                        .collect_vec()
                );
            }
        }
    }

    #[test]
    fn csv_noir_type() {
        for num_records in 0..100 {
            for terminator in &["\n", "\r\n"] {
                let file = NamedTempFile::new().unwrap();
                write!(file.as_file(), "a,b,c{terminator}").unwrap();
                for i in 0..num_records {
                    write!(file.as_file(), "{}{}", i, terminator).unwrap();
                }

                let mut env = StreamEnvironment::new(EnvironmentConfig::local(4));
                let source = RowCsvSource::new(file.path());
                let res = env.stream(source).map(NoirData::from).collect_vec();
                env.execute_blocking();

                let res = res.get().unwrap().into_iter().sorted().collect_vec();
                assert_eq!(
                    res,
                    (0..num_records)
                        .map(|x| NoirData::NoirType(NoirType::from(x)))
                        .collect_vec()
                );
            }
        }
    }
}
