use async_trait::async_trait;
use russh::client::{Config, Handle, Handler, Msg};
use russh::Channel;
use russh_keys::key::KeyPair;
use std::fs::File;
use std::io::{self, BufReader, Read};

use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

/// An authentification token, currently only by password.
///
/// Used when creating a [`Client`] for authentification.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuthMethod {
    Password(String),
    PrivateKey(String, Option<String>), // entire contents of private key file
    PrivateKeyFile(String, Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ServerCheckMethod {
    NoCheck,
    PublicKey(String), // base64 encoded key without the type prefix or hostname suffix (type is already encoded)
    PublicKeyFile(String),
    DefaultKnownHostsFile,
    KnownHostsFile(String),
}

impl AuthMethod {
    /// Convenience method to create a [`AuthMethod`] from a string literal.
    pub fn with_password(password: &str) -> Self {
        Self::Password(password.to_string())
    }

    pub fn with_key(key: &str, passphrase: Option<&str>) -> Self {
        Self::PrivateKey(key.to_string(), passphrase.map(str::to_string))
    }

    pub fn with_key_file(key_file_name: &str, passphrase: Option<&str>) -> Self {
        Self::PrivateKeyFile(key_file_name.to_string(), passphrase.map(str::to_string))
    }
}

impl ServerCheckMethod {
    /// Convenience method to create a [`ServerCheckMethod`] from a string literal.

    pub fn with_public_key(key: &str) -> Self {
        Self::PublicKey(key.to_string())
    }

    pub fn with_public_key_file(key_file_name: &str) -> Self {
        Self::PublicKeyFile(key_file_name.to_string())
    }

    pub fn with_known_hosts_file(known_hosts_file: &str) -> Self {
        Self::KnownHostsFile(known_hosts_file.to_string())
    }
}

pub struct ChannelHelper {
    ch: Channel<Msg>,
}

impl ChannelHelper {
    pub async fn execute(&mut self, command: &str) -> Result<CommandExecutedResult, crate::Error> {
        self.ch.exec(true, command).await?;
        let mut receive_buffer = vec![];
        while let Some(msg) = self.ch.wait().await {
            match msg {
                russh::ChannelMsg::Data { ref data } => {
                    std::io::Write::write_all(&mut receive_buffer, data).unwrap()
                }
                russh::ChannelMsg::ExitStatus { exit_status } => {
                    let result = CommandExecutedResult {
                        output: String::from_utf8_lossy(&receive_buffer).to_string(),
                        exit_status,
                    };
                    return Ok(result);
                }
                _ => {}
            }
        }

        Err(crate::Error::CommandDidntExit)
    }
}

/// A ssh connection to a remote server.
///
/// After creating a `Client` by [`connect`]ing to a remote host,
/// use [`execute`] to send commands and receive results through the connections.
///
/// [`connect`]: Client::connect
/// [`execute`]: Client::execute
///
/// # Examples
///
/// ```no_run
/// use async_ssh2_tokio::{Client, AuthMethod, ServerCheckMethod};
/// #[tokio::main]
/// async fn main() -> Result<(), async_ssh2_tokio::Error> {
///     let mut client = Client::connect(
///         ("10.10.10.2", 22),
///         "root",
///         AuthMethod::with_password("root"),
///         ServerCheckMethod::NoCheck,
///     ).await?;
///
///     let result = client.execute("echo Hello SSH").await?;
///     assert_eq!(result.output, "Hello SSH\n");
///     assert_eq!(result.exit_status, 0);
///
///     Ok(())
/// }
pub struct Client {
    connection_handle: Handle<ClientHandler>,
    username: String,
    address: SocketAddr,
}

impl Client {
    /// Open a ssh connection to a remote host.
    ///
    /// `addr` is an address of the remote host. Anything which implements
    /// [`ToSocketAddrs`] trait can be supplied for the address; see this trait
    /// documentation for concrete examples.
    ///
    /// If `addr` yields multiple addresses, `connect` will be attempted with
    /// each of the addresses until a connection is successful.
    /// Authentification is tried on the first successful connection and the whole
    /// process aborted if this fails.
    pub async fn connect(
        addr: impl ToSocketAddrs,
        username: &str,
        auth: AuthMethod,
        server_check: ServerCheckMethod,
    ) -> Result<Self, crate::Error> {
        Self::connect_with_config(addr, username, auth, server_check, Config::default()).await
    }

    /// Same as `connect`, but with the option to specify a non default
    /// [`russh::client::Config`].
    pub async fn connect_with_config(
        addr: impl ToSocketAddrs,
        username: &str,
        auth: AuthMethod,
        server_check: ServerCheckMethod,
        config: Config,
    ) -> Result<Self, crate::Error> {
        let config = Arc::new(config);

        // Connection code inspired from std::net::TcpStream::connect and std::net::each_addr
        let addrs = match addr.to_socket_addrs() {
            Ok(addrs) => addrs,
            Err(e) => return Err(crate::Error::AddressInvalid(e)),
        };
        let mut connect_res = Err(crate::Error::AddressInvalid(io::Error::new(
            io::ErrorKind::InvalidInput,
            "could not resolve to any addresses",
        )));
        for addr in addrs {
            let handler = ClientHandler {
                host: addr,
                server_check: server_check.clone(),
            };
            match russh::client::connect(config.clone(), addr, handler).await {
                Ok(h) => {
                    connect_res = Ok((addr, h));
                    break;
                }
                Err(e) => connect_res = Err(e),
            }
        }
        let (address, mut handle) = connect_res?;
        let username = username.to_string();

        Self::authenticate(&mut handle, &username, auth).await?;

        Ok(Self {
            connection_handle: handle,
            username,
            address,
        })
    }

    /// This takes a handle and performs authentification with the given method.
    async fn authenticate(
        handle: &mut Handle<ClientHandler>,
        username: &String,
        auth: AuthMethod,
    ) -> Result<(), crate::Error> {
        match auth {
            AuthMethod::Password(password) => {
                let is_authentificated = handle.authenticate_password(username, password).await?;
                if is_authentificated {
                    Ok(())
                } else {
                    Err(crate::Error::PasswordWrong)
                }
            }
            AuthMethod::PrivateKey(key_data, key_pass) => {
                let cprivk: KeyPair;
                if let Ok(kp) =
                    russh_keys::decode_secret_key(key_data.as_str(), key_pass.as_deref())
                {
                    cprivk = kp;
                } else {
                    return Err(crate::Error::KeyInvalid);
                }

                let is_authentificated = handle
                    .authenticate_publickey(username, Arc::new(cprivk))
                    .await?;
                if is_authentificated {
                    Ok(())
                } else {
                    Err(crate::Error::KeyAuthFailed)
                }
            }
            AuthMethod::PrivateKeyFile(key_file_name, key_pass) => {
                let cprivk: KeyPair;

                if let Ok(kp) = russh_keys::load_secret_key(key_file_name, key_pass.as_deref()) {
                    cprivk = kp;
                } else {
                    return Err(crate::Error::KeyInvalid);
                }

                let is_authentificated = handle
                    .authenticate_publickey(username, Arc::new(cprivk))
                    .await?;
                if is_authentificated {
                    Ok(())
                } else {
                    Err(crate::Error::KeyAuthFailed)
                }
            }
        }
    }

    /// Execute a remote command via the ssh connection.
    ///
    /// Returns both the stdout output and the exit code of the command,
    /// packaged in a [`CommandExecutedResult`] struct.
    /// If you need the stderr output, consider prefixing the command with a redirection,
    /// e.g. `2>&1 echo foo >>/dev/stderr`. If you don't need the output, use something like
    /// `echo foo >/dev/null`. Make sure your commands don't read from stdin and
    /// exit after bounded time.
    ///
    ///
    /// Can be called multiple times, but every invocation is a new shell context.
    /// Thus `cd`, setting variables and alike have no effect on future invocations.
    pub async fn execute(&mut self, command: &str) -> Result<CommandExecutedResult, crate::Error> {
        return match self.open_channel().await {
            Ok(mut helper) => helper.execute(command).await,
            Err(e) => Err(e),
        };
    }

    pub async fn file_transfer(
        &mut self,
        filepath: String,
    ) -> Result<CommandExecutedResult, crate::Error> {
        let channel = self.connection_handle.channel_open_session().await?;
        let mut stream = channel.into_stream();

        let mut file = File::open(filepath).unwrap();
        let mut reader = BufReader::new(file);

        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer);

        stream.write_all(&buffer).await;

        Ok(CommandExecutedResult {
            output: "Test".to_string(),
            exit_status: 0,
        })
    }
    pub async fn open_channel(&mut self) -> Result<ChannelHelper, crate::Error> {
        match self.connection_handle.channel_open_session().await {
            Ok(ch) => Ok(ChannelHelper { ch }),
            Err(e) => Err(crate::Error::SshError(e)),
        }
    }

    /// A debugging function to get the username this client is connected as.
    pub fn get_connection_username(&self) -> &String {
        &self.username
    }

    /// A debugging function to get the address this client is connected to.
    pub fn get_connection_address(&self) -> &SocketAddr {
        &self.address
    }

    pub async fn disconnect(&mut self) -> Result<(), russh::Error> {
        match self
            .connection_handle
            .disconnect(russh::Disconnect::ByApplication, "", "")
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CommandExecutedResult {
    /// The stdout output of the command.
    pub output: String,
    /// The unix exit status (`$?` in bash).
    pub exit_status: u32,
}

#[derive(Clone)]
struct ClientHandler {
    host: SocketAddr,
    server_check: ServerCheckMethod,
}

#[async_trait]
impl Handler for ClientHandler {
    type Error = crate::Error;

    async fn check_server_key(
        self,
        server_public_key: &russh_keys::key::PublicKey,
    ) -> Result<(Self, bool), Self::Error> {
        match &self.server_check {
            ServerCheckMethod::NoCheck => Ok((self, true)),
            ServerCheckMethod::PublicKey(key) => {
                let pk = russh_keys::parse_public_key_base64(key)
                    .map_err(|_| crate::Error::ServerCheckFailed)?;

                Ok((self, pk == *server_public_key))
            }
            ServerCheckMethod::PublicKeyFile(key_file_name) => {
                let pk = russh_keys::load_public_key(key_file_name)
                    .map_err(|_| crate::Error::ServerCheckFailed)?;

                Ok((self, pk == *server_public_key))
            }
            ServerCheckMethod::KnownHostsFile(known_hosts_path) => {
                let result = russh_keys::check_known_hosts_path(
                    &self.host.ip().to_string(),
                    self.host.port(),
                    server_public_key,
                    known_hosts_path,
                )
                .map_err(|_| crate::Error::ServerCheckFailed)?;

                Ok((self, result))
            }
            ServerCheckMethod::DefaultKnownHostsFile => {
                let result = russh_keys::check_known_hosts(
                    &self.host.ip().to_string(),
                    self.host.port(),
                    server_public_key,
                )
                .map_err(|_| crate::Error::ServerCheckFailed)?;

                Ok((self, result))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time;

    use crate::client::*;

    async fn establish_test_host_connection() -> Client {
        Client::connect(
            (
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                env!("ASYNC_SSH2_TEST_HOST_PORT").parse().unwrap(),
            ),
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_password(env!("ASYNC_SSH2_TEST_HOST_PW")),
            ServerCheckMethod::NoCheck,
        )
        .await
        .expect("Connection/Authentification failed")
    }

    #[tokio::test]
    async fn connect_with_password() {
        let client = establish_test_host_connection().await;
        assert_eq!(
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            client.get_connection_username(),
        );
        assert_eq!(
            concat!(
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                ":",
                env!("ASYNC_SSH2_TEST_HOST_PORT")
            )
            .parse(),
            Ok(*client.get_connection_address()),
        );
    }

    #[tokio::test]
    async fn execute_command_result() {
        let mut client = establish_test_host_connection().await;
        let output = client.execute("echo test!!!").await.unwrap();
        assert_eq!("test!!!\n", output.output);
        assert_eq!(0, output.exit_status);
    }

    #[tokio::test]
    async fn unicode_output() {
        let mut client = establish_test_host_connection().await;
        let output = client.execute("echo To thḙ moon! 🚀").await.unwrap();
        assert_eq!("To thḙ moon! 🚀\n", output.output);
        assert_eq!(0, output.exit_status);
    }

    #[tokio::test]
    async fn execute_command_status() {
        let mut client = establish_test_host_connection().await;
        let output = client.execute("exit 42").await.unwrap();
        assert_eq!(42, output.exit_status);
    }

    #[tokio::test]
    async fn execute_multiple_commands() {
        let mut client = establish_test_host_connection().await;
        let output = client.execute("echo test!!!").await.unwrap().output;
        assert_eq!("test!!!\n", output);

        let output = client.execute("echo Hello World").await.unwrap().output;
        assert_eq!("Hello World\n", output);
    }

    #[tokio::test]
    async fn stderr_redirection() {
        let mut client = establish_test_host_connection().await;

        let output = client.execute("echo foo >/dev/null").await.unwrap();
        assert_eq!("", output.output);

        let output = client.execute("echo foo >>/dev/stderr").await.unwrap();
        assert_eq!("", output.output);

        let output = client.execute("2>&1 echo foo >>/dev/stderr").await.unwrap();
        assert_eq!("foo\n", output.output);
    }

    #[tokio::test]
    async fn sequential_commands() {
        let mut client = establish_test_host_connection().await;

        for i in 0..30 {
            std::thread::sleep(time::Duration::from_millis(200));
            let res = client
                .execute(&format!("echo {i}"))
                .await
                .expect(&format!("Execution failed in iteration {i}"));
            assert_eq!(format!("{i}\n"), res.output);
        }
    }

    #[tokio::test]
    async fn execute_multiple_context() {
        // This is maybe not expected behaviour, thus documenting this via a test is important.
        let mut client = establish_test_host_connection().await;
        let output = client
            .execute("export VARIABLE=42; echo $VARIABLE")
            .await
            .unwrap()
            .output;
        assert_eq!("42\n", output);

        let output = client.execute("echo $VARIABLE").await.unwrap().output;
        assert_eq!("\n", output);
    }

    #[tokio::test]
    async fn connect_second_address() {
        let addresses = [
            SocketAddr::from(([127, 0, 0, 1], 23)),
            concat!(
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                ":",
                env!("ASYNC_SSH2_TEST_HOST_PORT")
            )
            .parse()
            .expect("invalid env var"),
        ];
        let client = Client::connect(
            &addresses[..],
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_password(env!("ASYNC_SSH2_TEST_HOST_PW")),
            ServerCheckMethod::NoCheck,
        )
        .await
        .expect("Resolution to second address failed");

        assert_eq!(
            concat!(
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                ":",
                env!("ASYNC_SSH2_TEST_HOST_PORT")
            )
            .parse(),
            Ok(*client.get_connection_address()),
        );
    }

    #[tokio::test]
    async fn connect_with_wrong_password() {
        let error = Client::connect(
            (
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                env!("ASYNC_SSH2_TEST_HOST_PORT").parse().unwrap(),
            ),
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_password("hopefully the wrong password"),
            ServerCheckMethod::NoCheck,
        )
        .await
        .err()
        .expect("Client connected with wrong password");

        match error {
            crate::Error::PasswordWrong => {}
            _ => panic!("Wrong error type"),
        }
    }

    #[tokio::test]
    async fn invalid_address() {
        let no_client = Client::connect(
            "this is definitely not an address",
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_password("hopefully the wrong password"),
            ServerCheckMethod::NoCheck,
        )
        .await;
        assert!(no_client.is_err());
    }

    #[tokio::test]
    async fn connect_to_wrong_port() {
        let no_client = Client::connect(
            (env!("ASYNC_SSH2_TEST_HOST_IP"), 23),
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_password(env!("ASYNC_SSH2_TEST_HOST_PW")),
            ServerCheckMethod::NoCheck,
        )
        .await;
        assert!(no_client.is_err());
    }

    #[tokio::test]
    #[ignore = "This times out only after 20 seconds"]
    async fn connect_to_wrong_host() {
        let no_client = Client::connect(
            "172.16.0.6:22",
            "xxx",
            AuthMethod::with_password("xxx"),
            ServerCheckMethod::NoCheck,
        )
        .await;
        assert!(no_client.is_err());
    }

    #[tokio::test]
    async fn auth_key_file() {
        let client = Client::connect(
            (
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                env!("ASYNC_SSH2_TEST_HOST_PORT").parse().unwrap(),
            ),
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_key_file(env!("ASYNC_SSH2_TEST_CLIENT_PRIV"), None),
            ServerCheckMethod::NoCheck,
        )
        .await;
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn auth_key_file_with_passphrase() {
        let client = Client::connect(
            (
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                env!("ASYNC_SSH2_TEST_HOST_PORT").parse().unwrap(),
            ),
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_key_file(
                env!("ASYNC_SSH2_TEST_CLIENT_PROT_PRIV"),
                Some(env!("ASYNC_SSH2_TEST_CLIENT_PROT_PASS")),
            ),
            ServerCheckMethod::NoCheck,
        )
        .await;
        if client.is_err() {
            println!("{:?}", client.err());
            panic!();
        }
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn auth_key_str() {
        let key = std::fs::read_to_string(env!("ASYNC_SSH2_TEST_CLIENT_PRIV")).unwrap();

        let client = Client::connect(
            (
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                env!("ASYNC_SSH2_TEST_HOST_PORT").parse().unwrap(),
            ),
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_key(key.as_str(), None),
            ServerCheckMethod::NoCheck,
        )
        .await;
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn auth_key_str_with_passphrase() {
        let key = std::fs::read_to_string(env!("ASYNC_SSH2_TEST_CLIENT_PROT_PRIV")).unwrap();

        let client = Client::connect(
            (
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                env!("ASYNC_SSH2_TEST_HOST_PORT").parse().unwrap(),
            ),
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_key(key.as_str(), Some(env!("ASYNC_SSH2_TEST_CLIENT_PROT_PASS"))),
            ServerCheckMethod::NoCheck,
        )
        .await;
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn server_check_file() {
        let client = Client::connect(
            (
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                env!("ASYNC_SSH2_TEST_HOST_PORT").parse().unwrap(),
            ),
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_password(env!("ASYNC_SSH2_TEST_HOST_PW")),
            ServerCheckMethod::with_public_key_file(env!("ASYNC_SSH2_TEST_SERVER_PUB")),
        )
        .await;
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn server_check_str() {
        let line = std::fs::read_to_string(env!("ASYNC_SSH2_TEST_SERVER_PUB")).unwrap();
        let mut split = line.split_whitespace();
        let key = match (split.next(), split.next()) {
            (Some(_), Some(k)) => k,
            (Some(k), None) => k,
            _ => panic!("Failed to parse pub key file"),
        };

        let client = Client::connect(
            (
                env!("ASYNC_SSH2_TEST_HOST_IP"),
                env!("ASYNC_SSH2_TEST_HOST_PORT").parse().unwrap(),
            ),
            env!("ASYNC_SSH2_TEST_HOST_USER"),
            AuthMethod::with_password(env!("ASYNC_SSH2_TEST_HOST_PW")),
            ServerCheckMethod::with_public_key(key),
        )
        .await;
        assert!(client.is_ok());
    }
}
