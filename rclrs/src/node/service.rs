use std::boxed::Box;
use std::ffi::CString;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use crate::error::{RclReturnCode, ToResult};
use crate::Node;
use crate::{rcl_bindings::*, RclrsError};

use rosidl_runtime_rs::Message;

use crate::node::publisher::MessageCow;

use parking_lot::{Mutex, MutexGuard};

// SAFETY: The functions accessing this type, including drop(), shouldn't care about the thread
// they are running in. Therefore, this type can be safely sent to another thread.
unsafe impl Send for rcl_service_t {}

/// Internal struct used by services.
pub struct ServiceHandle {
    handle: Mutex<rcl_service_t>,
    node_handle: Arc<Mutex<rcl_node_t>>,
    pub(crate) in_use_by_wait_set: Arc<AtomicBool>,
}

impl ServiceHandle {
    pub(crate) fn lock(&self) -> MutexGuard<rcl_service_t> {
        self.handle.lock()
    }
}

impl Drop for ServiceHandle {
    fn drop(&mut self) {
        let handle = self.handle.get_mut();
        let node_handle = &mut *self.node_handle.lock();
        // SAFETY: No preconditions for this function
        unsafe {
            rcl_service_fini(handle, node_handle);
        }
    }
}

/// Trait to be implemented by concrete Service structs.
///
/// See [`Service<T>`] for an example
pub trait ServiceBase: Send + Sync {
    /// Internal function to get a reference to the `rcl` handle.
    fn handle(&self) -> &ServiceHandle;
    /// Tries to take a new request and run the callback with it.
    fn execute(&self) -> Result<(), RclrsError>;
}

type ServiceCallback<Request, Response> =
    Box<dyn Fn(&rmw_request_id_t, Request) -> Response + 'static + Send>;

/// Main class responsible for responding to requests sent by ROS clients.
pub struct Service<T>
where
    T: rosidl_runtime_rs::Service,
{
    pub(crate) handle: Arc<ServiceHandle>,
    /// The callback function that runs when a request was received.
    pub callback: Mutex<ServiceCallback<T::Request, T::Response>>,
}

impl<T> Service<T>
where
    T: rosidl_runtime_rs::Service,
{
    /// Creates a new service.
    pub fn new<F>(node: &Node, topic: &str, callback: F) -> Result<Self, RclrsError>
    where
        T: rosidl_runtime_rs::Service,
        F: Fn(&rmw_request_id_t, T::Request) -> T::Response + 'static + Send,
    {
        // SAFETY: Getting a zero-initialized value is always safe.
        let mut service_handle = unsafe { rcl_get_zero_initialized_service() };
        let type_support = <T as rosidl_runtime_rs::Service>::get_type_support()
            as *const rosidl_service_type_support_t;
        let topic_c_string = CString::new(topic).map_err(|err| RclrsError::StringContainsNul {
            err,
            s: topic.into(),
        })?;
        let node_handle = &mut *node.rcl_node_mtx.lock();

        // SAFETY: No preconditions for this function.
        let service_options = unsafe { rcl_service_get_default_options() };

        unsafe {
            // SAFETY: The rcl_service is zero-initialized as expected by this function.
            // The rcl_node is kept alive because it is co-owned by the service.
            // The topic name and the options are copied by this function, so they can be dropped
            // afterwards.
            rcl_service_init(
                &mut service_handle as *mut _,
                node_handle as *mut _,
                type_support,
                topic_c_string.as_ptr(),
                &service_options as *const _,
            )
            .ok()?;
        }

        let handle = Arc::new(ServiceHandle {
            handle: Mutex::new(service_handle),
            node_handle: node.rcl_node_mtx.clone(),
            in_use_by_wait_set: Arc::new(AtomicBool::new(false)),
        });

        Ok(Self {
            handle,
            callback: Mutex::new(Box::new(callback)),
        })
    }

    /// Fetches a new request.
    ///
    /// When there is no new message, this will return a
    /// [`ServiceTakeFailed`][1].
    ///
    /// [1]: crate::RclrsError
    //
    // ```text
    // +---------------------+
    // | rclrs::take_request |
    // +----------+----------+
    //            |
    //            |
    // +----------v----------+
    // |  rcl_take_request   |
    // +----------+----------+
    //            |
    //            |
    // +----------v----------+
    // |      rmw_take       |
    // +---------------------+
    // ```
    pub fn take_request(&self) -> Result<(T::Request, rmw_request_id_t), RclrsError> {
        let mut request_id_out = rmw_request_id_t {
            writer_guid: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            sequence_number: 0,
        };
        type RmwMsg<T> =
            <<T as rosidl_runtime_rs::Service>::Request as rosidl_runtime_rs::Message>::RmwMsg;
        let mut request_out = RmwMsg::<T>::default();
        let handle = &*self.handle.lock();
        unsafe {
            // SAFETY: The three pointers are valid/initialized
            rcl_take_request(
                handle,
                &mut request_id_out,
                &mut request_out as *mut RmwMsg<T> as *mut _,
            )
        }
        .ok()?;
        Ok((T::Request::from_rmw_message(request_out), request_id_out))
    }
}

impl<T> ServiceBase for Service<T>
where
    T: rosidl_runtime_rs::Service,
{
    fn handle(&self) -> &ServiceHandle {
        &self.handle
    }

    fn execute(&self) -> Result<(), RclrsError> {
        let (req, mut req_id) = match self.take_request() {
            Ok((req, req_id)) => (req, req_id),
            Err(RclrsError::RclError {
                code: RclReturnCode::ServiceTakeFailed,
                ..
            }) => {
                // Spurious wakeup – this may happen even when a waitset indicated that this
                // service was ready, so it shouldn't be an error.
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let res = (*self.callback.lock())(&req_id, req);
        let rmw_message = <T::Response as Message>::into_rmw_message(res.into_cow());
        let handle = &*self.handle.lock();
        unsafe {
            // SAFETY: The response type is guaranteed to match the service type by the type system.
            rcl_send_response(
                handle,
                &mut req_id,
                rmw_message.as_ref() as *const <T::Response as Message>::RmwMsg as *mut _,
            )
        }
        .ok()
    }
}
