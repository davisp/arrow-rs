// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Integration tests for the FlightSQL server.

use arrow_flight::sql::server::PeekableFlightDataStream;
use arrow_flight::sql::DoPutPreparedStatementResult;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use core::str;
use futures::{stream, Stream, TryStreamExt};
use once_cell::sync::Lazy;
use prost::Message;
use std::collections::HashSet;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use tonic::metadata::MetadataValue;
use tonic::transport::Server;
use tonic::{Request, Response, Status, Streaming};

use arrow_array::builder::StringBuilder;
use arrow_array::{ArrayRef, RecordBatch};
use arrow_flight::encode::FlightDataEncoderBuilder;
use arrow_flight::sql::metadata::{
    SqlInfoData, SqlInfoDataBuilder, XdbcTypeInfo, XdbcTypeInfoData, XdbcTypeInfoDataBuilder,
};
use arrow_flight::sql::{
    server::FlightSqlService, ActionBeginSavepointRequest, ActionBeginSavepointResult,
    ActionBeginTransactionRequest, ActionBeginTransactionResult, ActionCancelQueryRequest,
    ActionCancelQueryResult, ActionClosePreparedStatementRequest,
    ActionCreatePreparedStatementRequest, ActionCreatePreparedStatementResult,
    ActionCreatePreparedSubstraitPlanRequest, ActionEndSavepointRequest,
    ActionEndTransactionRequest, Any, Command, CommandGetCatalogs, CommandGetCrossReference,
    CommandGetDbSchemas, CommandGetExportedKeys, CommandGetImportedKeys, CommandGetPrimaryKeys,
    CommandGetSqlInfo, CommandGetTableTypes, CommandGetTables, CommandGetXdbcTypeInfo,
    CommandPreparedStatementQuery, CommandPreparedStatementUpdate, CommandStatementIngest,
    CommandStatementQuery, CommandStatementSubstraitPlan, CommandStatementUpdate, Nullable,
    ProstMessageExt, Searchable, SqlInfo, TicketStatementQuery, XdbcDataType,
};
use arrow_flight::utils::batches_to_flight_data;
use arrow_flight::{
    flight_service_server::FlightService, flight_service_server::FlightServiceServer, Action,
    ActionType, FlightData, FlightDescriptor, FlightEndpoint, FlightInfo, HandshakeRequest,
    HandshakeResponse, IpcMessage, SchemaAsIpc, Ticket,
};
use arrow_ipc::writer::IpcWriteOptions;
use arrow_schema::{ArrowError, DataType, Field, Schema};

//type TonicStream<T> = Pin<Box<dyn Stream<Item = T> + Send + Sync + 'static>>;

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
type Result<T = (), E = Error> = std::result::Result<T, E>;

macro_rules! status {
    ($desc:expr, $err:expr) => {
        Status::internal(format!("{}: {} at {}:{}", $desc, $err, file!(), line!()))
    };
}

macro_rules! function_name {
    () => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        let name = type_name_of(f);
        name.strip_suffix("::f").unwrap()
    }};
}

const FAKE_TOKEN: &str = "uuid_token";
const FAKE_HANDLE: &str = "uuid_handle";
const FAKE_UPDATE_RESULT: i64 = 1;

static INSTANCE_SQL_DATA: Lazy<SqlInfoData> = Lazy::new(|| {
    let mut builder = SqlInfoDataBuilder::new();
    // Server information
    builder.append(SqlInfo::FlightSqlServerName, "Example Flight SQL Server");
    builder.append(SqlInfo::FlightSqlServerVersion, "1");
    // 1.3 comes from https://github.com/apache/arrow/blob/f9324b79bf4fc1ec7e97b32e3cce16e75ef0f5e3/format/Schema.fbs#L24
    builder.append(SqlInfo::FlightSqlServerArrowVersion, "1.3");
    builder.build().unwrap()
});

static INSTANCE_XBDC_DATA: Lazy<XdbcTypeInfoData> = Lazy::new(|| {
    let mut builder = XdbcTypeInfoDataBuilder::new();
    builder.append(XdbcTypeInfo {
        type_name: "INTEGER".into(),
        data_type: XdbcDataType::XdbcInteger,
        column_size: Some(32),
        literal_prefix: None,
        literal_suffix: None,
        create_params: None,
        nullable: Nullable::NullabilityNullable,
        case_sensitive: false,
        searchable: Searchable::Full,
        unsigned_attribute: Some(false),
        fixed_prec_scale: false,
        auto_increment: Some(false),
        local_type_name: Some("INTEGER".into()),
        minimum_scale: None,
        maximum_scale: None,
        sql_data_type: XdbcDataType::XdbcInteger,
        datetime_subcode: None,
        num_prec_radix: Some(2),
        interval_precision: None,
    });
    builder.build().unwrap()
});

static TABLES: Lazy<Vec<&'static str>> = Lazy::new(|| vec!["flight_sql.example.table"]);

/// Run a scenario that tests integration testing.
pub async fn scenario_setup(port: u16) -> Result {
    let addr = super::listen_on(port).await?;
    let svc = FlightServiceServer::new(FlightSqlServiceImpl {});
    let server = Server::builder().add_service(svc).serve(addr);
    println!("Server listening on localhost:{}", addr.port());
    server.await?;
    Ok(())
}

/// The FlightSqlServiceImpl for integration testing
#[derive(Clone)]
pub struct FlightSqlServiceImpl {}

impl FlightSqlServiceImpl {
    fn check_token<T>(&self, req: &Request<T>) -> Result<(), Status> {
        let metadata = req.metadata();
        let auth = metadata.get("authorization").ok_or_else(|| {
            Status::internal(format!("No authorization header! metadata = {metadata:?}"))
        })?;
        let str = auth
            .to_str()
            .map_err(|e| Status::internal(format!("Error parsing header: {e}")))?;
        let authorization = str.to_string();
        let bearer = "Bearer ";
        if !authorization.starts_with(bearer) {
            Err(Status::internal("Invalid auth header!"))?;
        }
        let token = authorization[bearer.len()..].to_string();
        if token == FAKE_TOKEN {
            Ok(())
        } else {
            Err(Status::unauthenticated("invalid token "))
        }
    }

    fn fake_result() -> Result<RecordBatch, ArrowError> {
        let schema = Schema::new(vec![Field::new("salutation", DataType::Utf8, false)]);
        let mut builder = StringBuilder::new();
        builder.append_value("Hello, FlightSQL!");
        let cols = vec![Arc::new(builder.finish()) as ArrayRef];
        RecordBatch::try_new(Arc::new(schema), cols)
    }
}

#[tonic::async_trait]
impl FlightSqlService for FlightSqlServiceImpl {
    type FlightService = FlightSqlServiceImpl;

    async fn do_handshake(
        &self,
        _request: Request<Streaming<HandshakeRequest>>,
    ) -> Result<
        Response<Pin<Box<dyn Stream<Item = Result<HandshakeResponse, Status>> + Send>>>,
        Status,
    > {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "Handshake has no default implementation",
        ))
    }

    /// Implementors may override to handle additional calls to do_get()
    async fn do_get_fallback(
        &self,
        _request: Request<Ticket>,
        message: Any,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(format!(
            "do_get: The defined request is invalid: {}",
            message.type_url
        )))
    }

    /// Get a FlightInfo for executing a SQL query.
    async fn get_flight_info_statement(
        &self,
        _query: CommandStatementQuery,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_statement has no default implementation",
        ))
    }

    /// Get a FlightInfo for executing a substrait plan.
    async fn get_flight_info_substrait_plan(
        &self,
        _query: CommandStatementSubstraitPlan,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_substrait_plan has no default implementation",
        ))
    }

    /// Get a FlightInfo for executing an already created prepared statement.
    async fn get_flight_info_prepared_statement(
        &self,
        _query: CommandPreparedStatementQuery,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_prepared_statement has no default implementation",
        ))
    }

    /// Get a FlightInfo for listing catalogs.
    async fn get_flight_info_catalogs(
        &self,
        _query: CommandGetCatalogs,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_catalogs has no default implementation",
        ))
    }

    /// Get a FlightInfo for listing schemas.
    async fn get_flight_info_schemas(
        &self,
        _query: CommandGetDbSchemas,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_schemas has no default implementation",
        ))
    }

    /// Get a FlightInfo for listing tables.
    async fn get_flight_info_tables(
        &self,
        _query: CommandGetTables,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_tables has no default implementation",
        ))
    }

    /// Get a FlightInfo to extract information about the table types.
    async fn get_flight_info_table_types(
        &self,
        _query: CommandGetTableTypes,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_table_types has no default implementation",
        ))
    }

    /// Get a FlightInfo for retrieving other information (See SqlInfo).
    async fn get_flight_info_sql_info(
        &self,
        _query: CommandGetSqlInfo,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_sql_info has no default implementation",
        ))
    }

    /// Get a FlightInfo to extract information about primary and foreign keys.
    async fn get_flight_info_primary_keys(
        &self,
        _query: CommandGetPrimaryKeys,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_primary_keys has no default implementation",
        ))
    }

    /// Get a FlightInfo to extract information about exported keys.
    async fn get_flight_info_exported_keys(
        &self,
        _query: CommandGetExportedKeys,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_exported_keys has no default implementation",
        ))
    }

    /// Get a FlightInfo to extract information about imported keys.
    async fn get_flight_info_imported_keys(
        &self,
        _query: CommandGetImportedKeys,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_imported_keys has no default implementation",
        ))
    }

    /// Get a FlightInfo to extract information about cross reference.
    async fn get_flight_info_cross_reference(
        &self,
        _query: CommandGetCrossReference,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_cross_reference has no default implementation",
        ))
    }

    /// Get a FlightInfo to extract information about the supported XDBC types.
    async fn get_flight_info_xdbc_type_info(
        &self,
        _query: CommandGetXdbcTypeInfo,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "get_flight_info_xdbc_type_info has no default implementation",
        ))
    }

    /// Implementors may override to handle additional calls to get_flight_info()
    async fn get_flight_info_fallback(
        &self,
        cmd: Command,
        _request: Request<FlightDescriptor>,
    ) -> Result<Response<FlightInfo>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(format!(
            "get_flight_info: The defined request is invalid: {}",
            cmd.type_url()
        )))
    }

    // do_get

    /// Get a FlightDataStream containing the query results.
    async fn do_get_statement(
        &self,
        _ticket: TicketStatementQuery,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_statement has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the prepared statement query results.
    async fn do_get_prepared_statement(
        &self,
        _query: CommandPreparedStatementQuery,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_prepared_statement has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the list of catalogs.
    async fn do_get_catalogs(
        &self,
        _query: CommandGetCatalogs,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_catalogs has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the list of schemas.
    async fn do_get_schemas(
        &self,
        _query: CommandGetDbSchemas,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_schemas has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the list of tables.
    async fn do_get_tables(
        &self,
        _query: CommandGetTables,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_tables has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the data related to the table types.
    async fn do_get_table_types(
        &self,
        _query: CommandGetTableTypes,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_table_types has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the list of SqlInfo results.
    async fn do_get_sql_info(
        &self,
        _query: CommandGetSqlInfo,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_sql_info has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the data related to the primary and foreign keys.
    async fn do_get_primary_keys(
        &self,
        _query: CommandGetPrimaryKeys,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_primary_keys has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the data related to the exported keys.
    async fn do_get_exported_keys(
        &self,
        _query: CommandGetExportedKeys,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_exported_keys has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the data related to the imported keys.
    async fn do_get_imported_keys(
        &self,
        _query: CommandGetImportedKeys,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_imported_keys has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the data related to the cross reference.
    async fn do_get_cross_reference(
        &self,
        _query: CommandGetCrossReference,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_cross_reference has no default implementation",
        ))
    }

    /// Get a FlightDataStream containing the data related to the supported XDBC types.
    async fn do_get_xdbc_type_info(
        &self,
        _query: CommandGetXdbcTypeInfo,
        _request: Request<Ticket>,
    ) -> Result<Response<<Self as FlightService>::DoGetStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_get_xdbc_type_info has no default implementation",
        ))
    }

    // do_put

    /// Implementors may override to handle additional calls to do_put()
    async fn do_put_fallback(
        &self,
        _request: Request<PeekableFlightDataStream>,
        message: Any,
    ) -> Result<Response<<Self as FlightService>::DoPutStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(format!(
            "do_put: The defined request is invalid: {}",
            message.type_url
        )))
    }

    /// Execute an update SQL statement.
    async fn do_put_statement_update(
        &self,
        _ticket: CommandStatementUpdate,
        _request: Request<PeekableFlightDataStream>,
    ) -> Result<i64, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_put_statement_update has no default implementation",
        ))
    }

    /// Execute a bulk ingestion.
    async fn do_put_statement_ingest(
        &self,
        _ticket: CommandStatementIngest,
        _request: Request<PeekableFlightDataStream>,
    ) -> Result<i64, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_put_statement_ingest has no default implementation",
        ))
    }

    /// Bind parameters to given prepared statement.
    ///
    /// Returns an opaque handle that the client should pass
    /// back to the server during subsequent requests with this
    /// prepared statement.
    async fn do_put_prepared_statement_query(
        &self,
        _query: CommandPreparedStatementQuery,
        _request: Request<PeekableFlightDataStream>,
    ) -> Result<DoPutPreparedStatementResult, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_put_prepared_statement_query has no default implementation",
        ))
    }

    /// Execute an update SQL prepared statement.
    async fn do_put_prepared_statement_update(
        &self,
        _query: CommandPreparedStatementUpdate,
        _request: Request<PeekableFlightDataStream>,
    ) -> Result<i64, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_put_prepared_statement_update has no default implementation",
        ))
    }

    /// Execute a substrait plan
    async fn do_put_substrait_plan(
        &self,
        _query: CommandStatementSubstraitPlan,
        _request: Request<PeekableFlightDataStream>,
    ) -> Result<i64, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_put_substrait_plan has no default implementation",
        ))
    }

    // do_action

    /// Implementors may override to handle additional calls to do_action()
    async fn do_action_fallback(
        &self,
        request: Request<Action>,
    ) -> Result<Response<<Self as FlightService>::DoActionStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::invalid_argument(format!(
            "do_action: The defined request is invalid: {:?}",
            request.get_ref().r#type
        )))
    }

    /// Add custom actions to list_actions() result
    async fn list_custom_actions(&self) -> Option<Vec<Result<ActionType, Status>>> {
        println!("Called: {}", function_name!());
        None
    }

    /// Create a prepared statement from given SQL statement.
    async fn do_action_create_prepared_statement(
        &self,
        _query: ActionCreatePreparedStatementRequest,
        _request: Request<Action>,
    ) -> Result<ActionCreatePreparedStatementResult, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_action_create_prepared_statement has no default implementation",
        ))
    }

    /// Close a prepared statement.
    async fn do_action_close_prepared_statement(
        &self,
        _query: ActionClosePreparedStatementRequest,
        _request: Request<Action>,
    ) -> Result<(), Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_action_close_prepared_statement has no default implementation",
        ))
    }

    /// Create a prepared substrait plan.
    async fn do_action_create_prepared_substrait_plan(
        &self,
        _query: ActionCreatePreparedSubstraitPlanRequest,
        _request: Request<Action>,
    ) -> Result<ActionCreatePreparedStatementResult, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_action_create_prepared_substrait_plan has no default implementation",
        ))
    }

    /// Begin a transaction
    async fn do_action_begin_transaction(
        &self,
        _query: ActionBeginTransactionRequest,
        _request: Request<Action>,
    ) -> Result<ActionBeginTransactionResult, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_action_begin_transaction has no default implementation",
        ))
    }

    /// End a transaction
    async fn do_action_end_transaction(
        &self,
        _query: ActionEndTransactionRequest,
        _request: Request<Action>,
    ) -> Result<(), Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_action_end_transaction has no default implementation",
        ))
    }

    /// Begin a savepoint
    async fn do_action_begin_savepoint(
        &self,
        _query: ActionBeginSavepointRequest,
        _request: Request<Action>,
    ) -> Result<ActionBeginSavepointResult, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_action_begin_savepoint has no default implementation",
        ))
    }

    /// End a savepoint
    async fn do_action_end_savepoint(
        &self,
        _query: ActionEndSavepointRequest,
        _request: Request<Action>,
    ) -> Result<(), Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_action_end_savepoint has no default implementation",
        ))
    }

    /// Cancel a query
    async fn do_action_cancel_query(
        &self,
        _query: ActionCancelQueryRequest,
        _request: Request<Action>,
    ) -> Result<ActionCancelQueryResult, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented(
            "do_action_cancel_query has no default implementation",
        ))
    }

    /// do_exchange
    /// Implementors may override to handle additional calls to do_exchange()
    async fn do_exchange_fallback(
        &self,
        _request: Request<Streaming<FlightData>>,
    ) -> Result<Response<<Self as FlightService>::DoExchangeStream>, Status> {
        println!("Called: {}", function_name!());
        Err(Status::unimplemented("Not yet implemented"))
    }

    async fn register_sql_info(&self, _id: i32, _result: &SqlInfo) {
        println!("Called: {}", function_name!());
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct FetchResults {
    #[prost(string, tag = "1")]
    pub handle: ::prost::alloc::string::String,
}

impl ProstMessageExt for FetchResults {
    fn type_url() -> &'static str {
        "type.googleapis.com/arrow.flight.protocol.sql.FetchResults"
    }

    fn as_any(&self) -> Any {
        Any {
            type_url: FetchResults::type_url().to_string(),
            value: ::prost::Message::encode_to_vec(self).into(),
        }
    }
}
