/// Android API wrapper handling Bluetooth classic RFCOMM/SPP connection.
///
/// TODO:
/// - Add a function and an enum for checking the bluetooth state.
/// - Add functions for permission request and enabling bluetooth.
///   <https://developer.android.com/develop/connectivity/bluetooth/setup>
/// - Add functions for device discovery and pairing.
use std::io::Error;

use jni::{
    objects::JObject,
    sys::{jint, jvalue},
};
use jni_min_helper::*;
use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

const BLUETOOTH_SERVICE: &str = "bluetooth";
pub const SPP_UUID: &str = "00001101-0000-1000-8000-00805F9B34FB";

/// Maps unexpected JNI errors to `std::io::Error` of `ErrorKind::Other`
/// (`From<jni::errors::Error>` cannot be implemented for `std::io::Error`
/// here because of the orphan rule). Side effect: `jni_last_cleared_ex()`.
#[inline(always)]
pub(crate) fn jerr(err: jni::errors::Error) -> Error {
    use jni::errors::Error::*;
    if let JavaException = err {
        let err = jni_clear_ex(err);
        jni_last_cleared_ex()
            .ok_or(JavaException)
            .and_then(|ex| Ok((ex, jni_attach_vm()?)))
            .and_then(|(ex, ref mut env)| Ok((ex.get_class_name(env)?, ex.get_throwable_msg(env)?)))
            .map(|(cls, msg)| Error::other(format!("{cls}: {msg}")))
            .unwrap_or(Error::other(err))
    } else {
        Error::other(err)
    }
}

/// Returns the global reference of the thread safe `android.bluetooth.BluetoothAdapter`,
/// created for once in this crate.
#[inline(always)]
pub(crate) fn bluetooth_adapter() -> Result<&'static JObject<'static>, Error> {
    use std::sync::OnceLock;
    static BT_ADAPTER: OnceLock<jni::objects::GlobalRef> = OnceLock::new();
    if let Some(ref_adapter) = BT_ADAPTER.get() {
        Ok(ref_adapter.as_obj())
    } else {
        let adapter = get_bluetooth_adapter()?;
        let _ = BT_ADAPTER.set(adapter.clone());
        Ok(BT_ADAPTER.get().unwrap().as_obj())
    }
}

// TODO: distinguish "unsupported on the device" and "permission denied".
fn get_bluetooth_adapter() -> Result<jni::objects::GlobalRef, Error> {
    let env = &mut jni_attach_vm().map_err(jerr)?;
    let context = android_context();

    let bluetooth_service = BLUETOOTH_SERVICE.new_jobject(env).map_err(jerr)?;
    let manager = env
        .call_method(
            context,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[(&bluetooth_service).into()],
        )
        .get_object(env)
        .map_err(jerr)?;
    if manager.is_null() {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "Cannot get BLUETOOTH_SERVICE",
        ));
    }
    let adapter = env
        .call_method(
            manager,
            "getAdapter",
            "()Landroid/bluetooth/BluetoothAdapter;",
            &[],
        )
        .get_object(env)
        .map_err(jerr)?;
    if !adapter.is_null() {
        Ok(env.new_global_ref(&adapter).map_err(jerr)?)
    } else {
        Err(Error::new(
            ErrorKind::Unsupported,
            "`getAdapter` returned null",
        ))
    }
}

/// Return true if Bluetooth is currently enabled and ready for use.
pub fn is_enabled() -> Result<bool, Error> {
    let adapter = bluetooth_adapter()?;
    let env = &mut jni_attach_vm().map_err(jerr)?;
    env.call_method(adapter, "isEnabled", "()Z", &[])
        .get_boolean()
        .map_err(jerr)
}

/// Gets a list of `BluetoothDevice` objects that are bonded (paired) to the adapter.
/// Returns an empty list if `is_enabled()` is false.
pub fn get_bonded_devices() -> Result<Vec<BluetoothDevice>, Error> {
    if !is_enabled()? {
        return Ok(Vec::new());
    }
    let adapter = bluetooth_adapter()?;
    let env = &mut jni_attach_vm().map_err(jerr)?;
    let dev_set = env
        .call_method(adapter, "getBondedDevices", "()Ljava/util/Set;", &[])
        .get_object(env)
        .map_err(jerr)?;
    if dev_set.is_null() {
        return Err(Error::from(ErrorKind::PermissionDenied));
    }
    let jarr = env
        .call_method(&dev_set, "toArray", "()[Ljava/lang/Object;", &[])
        .get_object(env)
        .map_err(jerr)?;
    let jarr: &jni::objects::JObjectArray = jarr.as_ref().into();
    let len = env.get_array_length(jarr).map_err(jerr)?;
    let mut vec = Vec::with_capacity(len as usize);
    for i in 0..len {
        vec.push(BluetoothDevice {
            internal: env
                .get_object_array_element(jarr, i)
                .global_ref(env)
                .map_err(jerr)?,
        });
    }
    Ok(vec)
}

/// Corresponds to `android.bluetooth.BluetoothDevice`.
#[derive(Clone, Debug)]
pub struct BluetoothDevice {
    pub(crate) internal: jni::objects::GlobalRef,
}

impl BluetoothDevice {
    /// Returns the hardware address of this BluetoothDevice.
    /// TODO: return some MAC address type instead of `String`.
    pub fn get_address(&self) -> Result<String, Error> {
        let env = &mut jni_attach_vm().map_err(jerr)?;
        env.call_method(&self.internal, "getAddress", "()Ljava/lang/String;", &[])
            .get_object(env)
            .map_err(jerr)?
            .get_string(env)
            .map_err(jerr)
    }

    /// Gets the friendly Bluetooth name of the remote device.
    /// It may return an error of `std::io::ErrorKind::PermissionDenied`, or some other error.
    pub fn get_name(&self) -> Result<String, Error> {
        let env = &mut jni_attach_vm().map_err(jerr)?;
        let dev_name = env
            .call_method(&self.internal, "getName", "()Ljava/lang/String;", &[])
            .get_object(env)
            .map_err(jerr)?;
        if dev_name.is_null() {
            return Err(Error::from(ErrorKind::PermissionDenied));
        }
        dev_name.get_string(env).map_err(jerr)
    }

    /// Creates the Android Bluetooth API socket object for RFCOMM communication.
    /// `connect` is not called automatically. `SPP_UUID` can be used.
    /// TODO: better error handling.
    pub fn build_rfcomm_socket(
        &self,
        uuid: &str,
        is_secure: bool,
    ) -> Result<BluetoothSocket, Error> {
        let env = &mut jni_attach_vm().map_err(jerr)?;

        let uuid = uuid.new_jobject(env).map_err(jerr)?;
        let uuid = env
            .call_static_method(
                "java/util/UUID",
                "fromString",
                "(Ljava/lang/String;)Ljava/util/UUID;",
                &[(&uuid).into()],
            )
            .get_object(env)
            .map_err(jerr)?; // TODO: distinguish IllegalArgumentException and others

        let method_name = if is_secure {
            "createRfcommSocketToServiceRecord"
        } else {
            "createInsecureRfcommSocketToServiceRecord"
        };
        env.call_method(
            &self.internal,
            method_name,
            "(Ljava/util/UUID;)Landroid/bluetooth/BluetoothSocket;",
            &[(&uuid).into()],
        )
        .get_object(env)
        .globalize(env)
        .map_err(jerr) // TODO: distinguish IOException and other unexpected exceptions
        .and_then(|internal| BluetoothSocket::build(internal))
    }
}

/// Manages the Bluetooth socket and IO streams. It uses a read buffer and a background thread,
/// because the timeout of the Java `InputStream` from the `BluetoothSocket` cannot be set.
/// The read timeout defaults to 0 (it does not block).
///
/// TODO: Improve error handling, provide some write success indicator and an `async` interface.
///
/// Reference:
/// <https://developer.android.com/develop/connectivity/bluetooth/transfer-data>
pub struct BluetoothSocket {
    internal: jni::objects::GlobalRef,

    input_stream: jni::objects::GlobalRef,
    buf_read: Arc<Mutex<VecDeque<u8>>>,
    thread_read: Option<JoinHandle<Result<(), Error>>>, // the returned value is unused
    read_callback: Arc<Mutex<Box<dyn Fn(Option<usize>) + 'static + Send>>>, // no-op by default
    read_timeout: Duration,                             // set for the standard Read trait

    output_stream: jni::objects::GlobalRef,
    jmethod_write: jni::objects::JMethodID,
    jmethod_flush: jni::objects::JMethodID,
    array_write: jni::objects::GlobalRef,
}

impl BluetoothSocket {
    const ARRAY_SIZE: usize = 32 * 1024;

    fn build(obj: jni::objects::GlobalRef) -> Result<Self, Error> {
        let env = &mut jni_attach_vm().map_err(jerr)?;

        // the streams may (or may NOT) be usable after reconnection (check Android SDK source)
        let input_stream = env
            .call_method(&obj, "getInputStream", "()Ljava/io/InputStream;", &[])
            .get_object(env)
            .globalize(env)
            .map_err(jerr)?;
        let output_stream = env
            .call_method(&obj, "getOutputStream", "()Ljava/io/OutputStream;", &[])
            .get_object(env)
            .globalize(env)
            .map_err(jerr)?;

        let jmethod_write = env
            .get_method_id("java/io/OutputStream", "write", "([BII)V")
            .map_err(jerr)?;
        let jmethod_flush = env
            .get_method_id("java/io/OutputStream", "flush", "()V")
            .map_err(jerr)?;

        let array_size = Self::ARRAY_SIZE as i32;
        let array_write = env
            .new_byte_array(array_size)
            .global_ref(env)
            .map_err(jerr)?;

        Ok(Self {
            internal: obj,

            input_stream,
            buf_read: Arc::new(Mutex::new(VecDeque::new())),
            thread_read: None,
            read_callback: Arc::new(Mutex::new(Box::new(|_| {}))),
            read_timeout: Duration::from_millis(0),

            output_stream,
            jmethod_write,
            jmethod_flush,
            array_write,
        })
    }

    /// Gets the connection status of this socket.
    pub fn is_connected(&self) -> Result<bool, Error> {
        let env = &mut jni_attach_vm().map_err(jerr)?;
        env.call_method(&self.internal, "isConnected", "()Z", &[])
            .get_boolean()
            .map_err(jerr)
    }

    /// Attempts to connect to a remote device. When connected, it creates a
    /// backgrond thread for reading data, which terminates itself on disconnection.
    /// Do not reuse the socket after disconnection, because the underlying OS
    /// implementation is probably incapable of reconnecting the device, just like
    /// `java.net.Socket`. TODO: improve error handling.
    pub fn connect(&mut self) -> Result<(), Error> {
        if self.is_connected()? {
            return Ok(());
        }
        let adapter = bluetooth_adapter()?;
        let env = &mut jni_attach_vm().map_err(jerr)?;

        let _ = env
            .call_method(&adapter, "cancelDiscovery", "()Z", &[])
            .clear_ex();
        env.call_method(&self.internal, "connect", "()V", &[])
            .clear_ex()
            .map_err(jerr)?;
        if self.is_connected()? {
            let socket = self.internal.clone();
            let input_stream = self.input_stream.clone();
            let arc_buf_read = self.buf_read.clone();
            let arc_callback = self.read_callback.clone();
            self.thread_read.replace(std::thread::spawn(move || {
                Self::read_loop(socket, input_stream, arc_buf_read, arc_callback)
            }));
            Ok(())
        } else {
            Err(Error::from(ErrorKind::NotConnected))
        }
    }

    /// Returns number of available bytes that can be read without blocking.
    pub fn len_available(&self) -> usize {
        self.buf_read.lock().unwrap().len()
    }

    /// Clears the managed read buffer used by the Rust side background thread.
    pub fn clear_read_buf(&mut self) {
        self.buf_read.lock().unwrap().clear();
    }

    /// Sets timeout for the `std::io::Read` implementation.
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = timeout;
    }

    /// Sets or replaces the callback to be invoked from the background thread when
    /// new data becomes available or the socket is disconnected. The length of newly
    /// arrived data instead of the length of available data in the read buffer will
    /// be passed to the callback (or `None` if it is disconnected).
    ///
    /// Note: Do not set a new callback inside the callback (it causes dead lock).
    pub fn set_read_callback(&mut self, f: impl Fn(Option<usize>) + 'static + Send) {
        *self.read_callback.lock().unwrap() = Box::new(f);
    }

    /// Closes this socket and releases any system resources associated with it.
    /// If the stream is already closed then invoking this method has no effect.
    pub fn close(&mut self) -> Result<(), Error> {
        if !self.is_connected()? {
            return Ok(());
        }
        let _ = self.flush();
        let env = &mut jni_attach_vm().map_err(jerr)?;
        env.call_method(&self.internal, "close", "()V", &[])
            .clear_ex()
            .map_err(jerr)?;
        if let Some(th) = self.thread_read.take() {
            let _ = th.join();
        }
        Ok(())
    }
}

impl BluetoothSocket {
    fn read_loop(
        socket: jni::objects::GlobalRef,
        input_stream: jni::objects::GlobalRef,
        buf_read: Arc<Mutex<VecDeque<u8>>>,
        read_callback: Arc<Mutex<Box<dyn Fn(Option<usize>) + 'static + Send>>>,
    ) -> Result<(), Error> {
        let env = &mut jni_attach_vm().map_err(jerr)?;
        let jmethod_read = env
            .get_method_id("java/io/InputStream", "read", "([BII)I")
            .map_err(jerr)?;
        let read_size = env
            .call_method(&socket, "getMaxReceivePacketSize", "()I", &[])
            .get_int()
            .map(|i| i as usize)
            .unwrap_or(Self::ARRAY_SIZE);

        let mut vec_read = vec![0u8; read_size];
        let array_read = env
            .new_byte_array(read_size as i32)
            .auto_local(env)
            .map_err(jerr)?;
        let array_read: &jni::objects::JByteArray<'_> = array_read.as_ref().into();

        loop {
            use jni::signature::*;
            // Safety: arguments passed to `call_method_unchecked` are correct.
            let read_len = unsafe {
                env.call_method_unchecked(
                    &input_stream,
                    jmethod_read,
                    ReturnType::Primitive(Primitive::Int),
                    &[
                        jvalue {
                            l: array_read.as_raw(),
                        },
                        jvalue { i: 0 as jint },
                        jvalue {
                            i: read_size as jint,
                        },
                    ],
                )
            }
            .get_int();
            if let Ok(len) = read_len {
                let len = if len > 0 {
                    len as usize
                } else {
                    continue;
                };
                // Safety: casts `&mut [u8]` to `&mut [i8]` for `get_byte_array_region`,
                // `input_stream.read(..)` = `len` <= `read_size` = `vec_read.len()`.
                let tmp_read = unsafe {
                    std::slice::from_raw_parts_mut(vec_read.as_mut_ptr() as *mut i8, len)
                };
                env.get_byte_array_region(&array_read, 0, tmp_read)
                    .map_err(jerr)?;
                buf_read.lock().unwrap().write(&vec_read[..len]).unwrap();
                read_callback.lock().unwrap()(Some(len));
            } else {
                if let Some(ex) = jni_last_cleared_ex() {
                    let ex_msg = ex.get_throwable_msg(env).unwrap().to_lowercase();
                    if ex_msg.contains("closed") {
                        let _ = env
                            .call_method(&socket, "close", "()V", &[])
                            .map_err(jni_clear_ex_ignore);
                        read_callback.lock().unwrap()(None);
                        return Ok(());
                    }
                }
                let is_connected = env
                    .call_method(&socket, "isConnected", "()Z", &[])
                    .get_boolean()
                    .map_err(jerr)?;
                if !is_connected {
                    read_callback.lock().unwrap()(None);
                    return Ok(());
                }
            }
        }
    }
}

impl Read for BluetoothSocket {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.len() == 0 {
            return Ok(0);
        }

        let t_timeout = SystemTime::now() + self.read_timeout;

        let mut cnt_read = 0;
        let mut disconnected = false;
        while cnt_read < buf.len() {
            let mut lck_buf_read = self.buf_read.lock().unwrap();
            if let Ok(cnt) = lck_buf_read.read(&mut buf[cnt_read..]) {
                cnt_read += cnt;
            }
            drop(lck_buf_read);
            if cnt_read >= buf.len() {
                break;
            } else if !self.is_connected()? {
                disconnected = true;
                break;
            } else if let Ok(dur_rem) = t_timeout.duration_since(SystemTime::now()) {
                std::thread::sleep(Duration::from_millis(100).min(dur_rem));
            } else {
                break;
            }
        }

        if cnt_read > 0 {
            Ok(cnt_read)
        } else if !disconnected {
            Err(Error::from(ErrorKind::TimedOut))
        } else {
            Err(Error::from(ErrorKind::NotConnected))
        }
    }
}

impl Write for BluetoothSocket {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if buf.len() == 0 {
            return Ok(0);
        }

        let env = &mut jni_attach_vm().map_err(jerr)?;
        let array_write: &jni::objects::JByteArray<'_> = self.array_write.as_obj().into();
        if (env.get_array_length(array_write).map_err(jerr)? as usize) < buf.len() {
            // replace the prepared reusable Java array with a larger array
            self.array_write = env
                .byte_array_from_slice(buf)
                .global_ref(env)
                .map_err(jerr)?;
        } else {
            // Safety: casts `&[u8]` to `&[i8]` for `set_byte_array_region`.
            let buf = unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const i8, buf.len()) };
            env.set_byte_array_region(array_write, 0, buf)
                .map_err(jerr)?;
        }

        use jni::signature::*;
        // Safety: arguments passed to `call_method_unchecked` are correct.
        unsafe {
            env.call_method_unchecked(
                &self.output_stream,
                self.jmethod_write,
                ReturnType::Primitive(Primitive::Void),
                &[
                    jvalue {
                        l: self.array_write.as_raw(),
                    },
                    jvalue { i: 0 as jint },
                    jvalue {
                        i: buf.len() as jint,
                    },
                ],
            )
        }
        .clear_ex()
        .map_err(|e| {
            if !self.is_connected().unwrap_or(false) {
                Error::from(ErrorKind::NotConnected)
            } else {
                jerr(e)
            }
        })
        .map(|_| buf.len()) // TODO: do some test to find possible size limit
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let env = &mut jni_attach_vm().map_err(jerr)?;
        use jni::signature::*;
        unsafe {
            env.call_method_unchecked(
                &self.output_stream,
                self.jmethod_flush,
                ReturnType::Primitive(Primitive::Void),
                &[],
            )
        }
        .clear_ex()
        .map_err(jerr)
    }
}

impl Drop for BluetoothSocket {
    fn drop(&mut self) {
        let _ = self.close();
    }
}
