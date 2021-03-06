
use futures::{
    Async,
    AsyncSink,
    Future,
    Poll,
    Sink,
    Stream
};
use futures::future::{
    Shared
};
use futures::stream::{
    SplitSink,
    SplitStream
};
use futures::sync::{
    mpsc,
    oneshot
};
use lsp_rs::{
    ClientNotification,
    IncomingMessage,
    IncomingServerMessage,
    MessageEnvelope,
    NotificationMessage,
    OutgoingMessage,
    OutgoingServerMessage,
    ResponseError,
    ResponseMessage,
    RequestMessage,
    ServerCodec,
    ServerNotification,
    ServerResponse,
    ServerRequest
};
use std::{
    io
};
use std::cell::{
    RefCell
};
use std::collections::{
    HashMap
};
use std::rc::{
    Rc
};
use tokio_core::io::{
    Framed,
    Io
};
use tokio_core::reactor::{
    Handle,
    Remote
};

type IoRead< I : Io >    = SplitStream< Framed< I, ServerCodec > >;
type IoWrite< I : Io >   = SplitSink< Framed< I, ServerCodec > >;

type CommandQueueSend    = mpsc::Sender< ServiceCommand >;
type CommandQueueRead    = mpsc::Receiver< ServiceCommand >;

type ResponseChannelSend = oneshot::Sender< ResponseMessage< ServerResponse > >;
type ResponseChannelRead = oneshot::Receiver< ResponseMessage< ServerResponse > >;

type ResponseQueueSend   = mpsc::Sender< ResponseChannelRead >;
type ResponseQueueRead   = mpsc::Receiver< ResponseChannelRead >;

type WriteQueueSend      = mpsc::Sender< OutgoingServerMessage >;
type WriteQueueRead      = mpsc::Receiver< OutgoingServerMessage >;

macro_rules! try_poll {
    (
        $e : expr
    ) => {
        match $e {
            Ok( Async::Ready( val ) ) => val,
            Ok( Async::NotReady ) => return Ok( Async::NotReady ),
            Err( error ) => return Err( error )
        }
    };
}

/// Main trait implemented by creators of this service to handle incoming requests and notifications
pub trait MessageHandler {

    /// Trait method called when a new RequestMessage has been received from the client
    ///
    /// This method does not have to respond before returning and can complete the request asynchronously,
    /// responses will be properly ordered when they are completed. This method should not block as it
    /// will block the IO thread and prevent other messages from being processed.
    fn handle_request( &self, service : ServiceHandle, request : ServerRequest, output : ResponseOutput );

    /// Trait method called when a new NotificationMessage has been received from the client.
    ///
    /// This method should not block as it will block the IO thread and prevent other messages from being
    /// processed.
    fn handle_notification( &self, service : ServiceHandle, notification : ServerNotification );

}

/// Struct that allows replying to a specific request. This struct is Send, allowing requests to be processed
/// within another thread if needed.
pub struct ResponseOutput {
    request_id     : i64,
    result_channel : ResponseChannelSend
}

/// Future that completes when the service is shutdown and no future requests shall be handled
#[derive( Clone )]
pub struct ShutdownFuture {
    shared_future : Shared< oneshot::Receiver< Result< ( ), ServiceError > > >
}

/// A handle to the service to send notifications or to shutdown the running service.
#[derive( Clone )]
pub struct ServiceHandle {
    shutdown_future : ShutdownFuture,
    command_send    : CommandQueueSend,

    remote_handle   : Remote
}

/// Errors generated by the service while reading/writing messages or processing requests
#[derive( Clone, Debug )]
pub enum ServiceError {
    /// Error type generated when there was an IO Error reading from the incoming stream
    ReadError( Rc< io::Error > ),
    /// Error type generated when there was an IO Error writing to the outgoing stream
    WriteError( Rc< io::Error > ),
    /// Error type generated when the service is unsure of the cause of error.
    ///
    /// Can be generated by:
    ///     Unexpected end of internal queues
    ///     Unable to add elements to internal queues
    Unknown
}

struct Service {
    shutdown_send : RefCell< Option< oneshot::Sender< Result< ( ), ServiceError > > > >,
    shutdown_read : ShutdownFuture,

    command_send  : CommandQueueSend,

    core_handle   : Handle
}

enum ServiceCommand {
    SendNotification( ClientNotification ),
    Shutdown
}

struct MessageReader< H : MessageHandler + 'static, I : Io + 'static > {
    service_handle      : ServiceHandle,

    io_read             : IoRead< I >,
    response_queue_send : ResponseQueueSend,
    current_request     : Option< ResponseChannelRead >,

    message_handler     : H
}

struct ResponseWriter {
    response_queue_read : ResponseQueueRead,
    write_queue_send    : WriteQueueSend,

    response_future     : Option< ResponseChannelRead >,
    response            : Option< OutgoingServerMessage >
}

struct CommandHandler {
    service_handle       : Rc< Service >,
    command_queue_read   : CommandQueueRead,
    write_queue_send     : WriteQueueSend,

    current_notification : Option< OutgoingServerMessage >
}

/// Creates a new service running on the specific tokio Handle, reading and writing messages to the given IO
/// stream, and using the provided MessageHandler to handle incoming messages.
///
/// This method spawns several futures on the given handle, and will not process any requests or responses
/// unless the event loop of the provided Handle is pumped.
///
/// Returns a ServiceHandle to the provided service. Dropping this handle will not shutdown the server.
pub fn start_service< H : MessageHandler + 'static, I : Io + 'static >( handle : Handle, message_handler : H, io : I ) -> ServiceHandle {
    Service::new( handle, message_handler, io )
}

impl Future for ShutdownFuture {

    type Item  = ( );
    type Error = ServiceError;

    fn poll( &mut self ) -> Poll< Self::Item, Self::Error > {
        let result = match self.shared_future.poll( ) {
            Ok( Async::Ready( result ) ) => result,
            Ok( Async::NotReady ) => return Ok( Async::NotReady ),
            Err( _ ) => {
                error!( "Unknown error occured while polling for shutdown." );

                return Err( ServiceError::Unknown )
            }
        };

        match *result {
            Ok( _ ) => Ok( Async::Ready( ( ) ) ),
            Err( ref error ) => Err( error.clone( ) )
        }
    }

}

impl ResponseOutput {

    pub fn send_result( self, result : ServerResponse ) {
        let request_id = self.request_id;

        self.complete( ResponseMessage {
            id     : request_id,
            result : Some( result ),
            error  : None
        } );
    }

    pub fn send_error( self, error : ResponseError ) {
        let request_id = self.request_id;

        self.complete( ResponseMessage {
            id     : request_id,
            result : None,
            error  : Some( error )
        } );
    }

    fn complete( self, response : ResponseMessage< ServerResponse > ) {
        let ResponseOutput { request_id , result_channel } = self;
        trace!( "Completing request {} with response {:?}", request_id, response );

        result_channel.complete( response );
    }

}

impl ServiceHandle {

    pub fn get_shutdown_future( &self ) -> &ShutdownFuture {
        &self.shutdown_future
    }

    pub fn shutdown( &self ) {
        let moved_command_send = self.command_send.clone( );
        self.remote_handle.spawn( move | _ | {
            moved_command_send.send( ServiceCommand::Shutdown ).then( | _ | {
                Ok( ( ) )
            } )
        } );
    }

    pub fn send_notification( &self, notification : ClientNotification ) {
        let moved_command_send = self.command_send.clone( );
        self.remote_handle.spawn( move | _ | {
            moved_command_send.send( ServiceCommand::SendNotification( notification ) ).then( | _ | {
                Ok( ( ) )
            } )
        } );
    }

}

impl Service {

    fn new< H : MessageHandler + 'static, I : Io + 'static >( core_handle : Handle, message_handler : H, io : I ) -> ServiceHandle {
        let ( response_queue_send, response_queue_read ) = mpsc::channel( 1024 );
        let ( write_queue_send, write_queue_read ) = mpsc::channel( 1024 );
        let ( shutdown_send, shutdown_read ) = oneshot::channel( );
        let ( command_send, command_read ) = mpsc::channel( 16 );

        let ( io_write, io_read ) = io.framed( ServerCodec::new( ) ).split( );

        let shutdown_future = ShutdownFuture {
            shared_future : shutdown_read.shared( )
        };

        let service = Rc::new( Service {
            shutdown_send : RefCell::new( Some( shutdown_send ) ),
            shutdown_read : shutdown_future.clone( ),

            command_send  : command_send.clone( ),

            core_handle   : core_handle
        } );
        let service_handle = ServiceHandle {
            shutdown_future : shutdown_future,
            command_send    : command_send,

            remote_handle   : service.core_handle.remote( ).clone( )
        };

        Service::spawn_message_reader( service.clone( ), service_handle.clone( ), io_read, response_queue_send, message_handler );
        Service::spawn_response_writer( service.clone( ), response_queue_read, write_queue_send.clone( ) );
        Service::spawn_message_writer( service.clone( ), write_queue_read, io_write );
        Service::spawn_command_handler( service.clone( ), command_read, write_queue_send );

        service_handle
    }

    fn spawn_message_reader< H : MessageHandler + 'static, I : Io + 'static >( this : Rc< Self >, service_handle : ServiceHandle, io_read : IoRead< I >, response_queue_send : ResponseQueueSend, message_handler : H ) {
        let reader = MessageReader::new( service_handle, io_read, response_queue_send, message_handler );

        Service::spawn_handler_future( this, reader );
    }

    fn spawn_message_writer< I : Io + 'static >( this : Rc< Self >, write_queue_read : WriteQueueRead, io_write : IoWrite< I > ) {
        let write_queue_read_map = write_queue_read.map( | message | {
            MessageEnvelope {
                headers : HashMap::new( ),
                message : message
            }
        } ).map_err( | _ | {
            io::Error::new( io::ErrorKind::Other, "Error reading from write queue." )
        } );
        let writer = io_write.send_all( write_queue_read_map ).map( | _ | {
            ( )
        } ).map_err( | err | {
            ServiceError::WriteError( Rc::new( err ) )
        } );

        Service::spawn_handler_future( this, writer );
    }

    fn spawn_response_writer( this : Rc< Self >, response_queue_read : ResponseQueueRead, write_queue_send : WriteQueueSend ) {
        let writer = ResponseWriter::new( response_queue_read, write_queue_send );

        Service::spawn_handler_future( this, writer );
    }

    fn spawn_command_handler( this : Rc< Self >, command_queue_read : CommandQueueRead, write_queue_send : WriteQueueSend ) {
        let handler = CommandHandler::new( this.clone( ), command_queue_read, write_queue_send );

        Service::spawn_handler_future( this, handler );
    }

    fn spawn_handler_future< F >( this : Rc< Self >, f : F ) where F : Future< Item = ( ), Error = ServiceError > + 'static {
        let our_this = this.clone( );

        let mapped_err = f.map_err( move | service_err | {
            this.shutdown_error( service_err );

            ( )
        } );

        let shutdown_notif = our_this.shutdown_read.clone( ).then( | _ | {
            Ok( ( ) )
        } );

        let select = mapped_err.select( shutdown_notif ).map( | _ | {
            ( )
        } ).map_err( | _ | {
            ( )
        } );

        our_this.spawn( select );
    }

    fn spawn< F >( &self, f : F ) where F : Future< Item = ( ), Error = ( ) > + 'static {
        self.core_handle.spawn( f );
    }

    fn shutdown( &self ) {
        let channel = self.shutdown_send.borrow_mut( ).take( );
        match channel {
            Some( channel ) => {
                trace!( "Shutting down service." );

                channel.complete( Ok( ( ) ) );
            },
            None => { }
        }
    }

    fn shutdown_error( &self, error : ServiceError ) {
        let channel = self.shutdown_send.borrow_mut( ).take( );
        match channel {
            Some( channel ) => {
                error!( "Server shutting down with error {:?}", error );

                channel.complete( Err( error ) )
            },
            None => { }
        }
    }

}

impl < H : MessageHandler + 'static, I : Io + 'static > MessageReader< H, I > {

    fn new( service_handle : ServiceHandle, io_read : IoRead< I >, response_queue_send : ResponseQueueSend, message_handler : H ) -> Self {
        MessageReader {
            service_handle      : service_handle,

            io_read             : io_read,
            response_queue_send : response_queue_send,
            current_request     : None,

            message_handler     : message_handler
        }
    }

    fn next_message( &mut self ) -> Poll< IncomingServerMessage, ServiceError > {
        match self.io_read.poll( ) {
            Ok( Async::Ready( Some( val ) ) ) => Ok( Async::Ready( val.message ) ),
            Ok( Async::Ready( None ) ) => {
                error!( "Incoming stream out of messages." );

                Err( ServiceError::Unknown )
            },
            Ok( Async::NotReady ) => Ok( Async::NotReady ),
            Err( error ) => Err( ServiceError::ReadError( Rc::new( error ) ) )
        }
    }

    fn push_response_future( &mut self, response_future : ResponseChannelRead ) -> Poll< ( ), ServiceError > {
        match self.response_queue_send.start_send( response_future ) {
            Ok( AsyncSink::Ready ) => Ok( Async::Ready( ( ) ) ),
            Ok( AsyncSink::NotReady( response_future ) ) => {
                self.current_request = Some( response_future );

                Ok( Async::NotReady )
            },
            Err( _ ) => {
                error!( "Error pushing response future to response channel." );

                Err( ServiceError::Unknown )
            }
        }
    }

}

impl < H : MessageHandler + 'static, I : Io + 'static > Future for MessageReader< H, I > {

    type Item  = ( );
    type Error = ServiceError;

    fn poll( &mut self ) -> Poll< Self::Item, Self::Error > {
        loop {
            if let Some( current_response ) = self.current_request.take( ) {
                try_poll!( self.push_response_future( current_response ) );
            }

            let message = try_poll!( self.next_message( ) );
            match message {
                IncomingMessage::Request( request ) => {
                    trace!( "Received request message: {:?}", request );

                    let RequestMessage{ id, method } = request;

                    let ( response_send, response_read ) = oneshot::channel( );
                    let output = ResponseOutput {
                        request_id     : id,
                        result_channel : response_send
                    };

                    self.message_handler.handle_request( self.service_handle.clone( ), method, output );
                    self.current_request = Some( response_read );
                },
                IncomingMessage::Notification( notification ) => {
                    trace!( "Received notification message: {:?}", notification );

                    self.message_handler.handle_notification( self.service_handle.clone( ), notification.method );
                },
                IncomingMessage::Response( response ) => {
                    unimplemented!( );
                }
            }
        }
    }

}

impl ResponseWriter {

    fn new( response_queue_read : ResponseQueueRead, write_queue_send : WriteQueueSend ) -> Self {
        ResponseWriter {
            response_queue_read : response_queue_read,
            write_queue_send    : write_queue_send,

            response_future     : None,
            response            : None
        }
    }

    fn poll_for_response_future( &mut self ) -> Poll< ResponseChannelRead, ServiceError > {
        match self.response_queue_read.poll( ) {
            Ok( Async::Ready( Some( response_future ) ) ) => Ok( Async::Ready( response_future ) ),
            Ok( Async::Ready( None ) ) => {
                error!( "Response channel unexpectedly closed." );

                Err( ServiceError::Unknown )
            },
            Ok( Async::NotReady ) => Ok( Async::NotReady ),
            Err( _ ) => {
                error!( "Error reading from response queue." );

                Err( ServiceError::Unknown )
            }
        }
    }

    fn poll_for_response( &mut self, mut response_future : ResponseChannelRead ) -> Poll< ( ), ServiceError > {
        let response = match response_future.poll( ) {
            Ok( Async::Ready( response ) ) => response,
            Ok( Async::NotReady ) => {
                self.response_future = Some( response_future );

                return Ok( Async::NotReady );
            },
            // Sender was dropped, assume request canceled
            Err( _ ) => return Ok( Async::Ready( ( ) ) )
        };

        self.response = Some( OutgoingMessage::Response( response ) );
        Ok( Async::Ready( ( ) ) )
    }

    fn write_response( &mut self, response : OutgoingServerMessage ) -> Poll< ( ), ServiceError > {
        match self.write_queue_send.start_send( response ) {
            Ok( AsyncSink::Ready ) => Ok( Async::Ready( ( ) ) ),
            Ok( AsyncSink::NotReady( response ) ) => {
                self.response = Some( response );

                Ok( Async::NotReady )
            },
            Err( _ ) => {
                error!( "Error writing response to write queue." );

                Err( ServiceError::Unknown )
            }
        }
    }

}

impl Future for ResponseWriter {

    type Item  = ( );
    type Error = ServiceError;

    fn poll( &mut self ) -> Poll< Self::Item, Self::Error > {
        loop {
            if let Some( response_future ) = self.response_future.take( ) {
                try_poll!( self.poll_for_response( response_future ) );
            }
            if let Some( response ) = self.response.take( ) {
                try_poll!( self.write_response( response ) );
            }

            self.response_future = Some( try_poll!( self.poll_for_response_future( ) ) );
        }
    }

}

impl CommandHandler {

    fn new( service_handle : Rc< Service >, command_queue_read : CommandQueueRead, write_queue_send : WriteQueueSend ) -> Self {
        CommandHandler {
            service_handle       : service_handle,
            command_queue_read   : command_queue_read,
            write_queue_send     : write_queue_send,

            current_notification : None
        }
    }

}

impl Future for CommandHandler {

    type Item  = ( );
    type Error = ServiceError;

    fn poll( &mut self ) -> Poll< Self::Item, Self::Error > {
        loop {
            if let Some( notification ) = self.current_notification.take( ) {
                match self.write_queue_send.start_send( notification ) {
                    Ok( AsyncSink::Ready ) => { },
                    Ok( AsyncSink::NotReady( notification ) ) => {
                        self.current_notification = Some( notification );

                        return Ok( Async::NotReady );
                    },
                    Err( _ ) => {
                        error!( "Error sending notification to write queue." );

                        return Err( ServiceError::Unknown )
                    }
                }
            }

            let command = match self.command_queue_read.poll( ) {
                Ok( Async::Ready( Some( command ) ) ) => command,
                Ok( Async::Ready( None ) ) => {
                    error!( "Unexpected end of command queue." );

                    return Err( ServiceError::Unknown )
                },
                Ok( Async::NotReady ) => return Ok( Async::NotReady ),
                Err( _ ) => {
                    error!( "Error reading from command queue." );

                    return Err( ServiceError::Unknown )
                }
            };

            match command {
                ServiceCommand::Shutdown => {
                    self.service_handle.shutdown( );

                    return Ok( Async::NotReady );
                },
                ServiceCommand::SendNotification( notification ) => {
                    self.current_notification = Some( OutgoingMessage::Notification( NotificationMessage { method : notification } ) );
                }
            }
        }
    }

}