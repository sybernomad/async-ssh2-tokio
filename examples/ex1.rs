use std::sync::Arc;

use async_ssh2_tokio::client::{AuthMethod, Client, ServerCheckMethod, CommandExecutedResult};
use tokio::task::{self, JoinSet};
use dashmap::DashMap;
use std::io::{self, Write};

struct Tester{
    node_conns: Arc<DashMap<u32, async_ssh2_tokio::Client>>,
}

impl Tester {
    fn new()->Tester{
        return Tester{node_conns: Arc::new(DashMap::new())};
    }

    async fn connect(&self){
        let client = Client::connect(
            ("127.0.0.1", 2222),
            "test",
            AuthMethod::with_password("test"),
            ServerCheckMethod::NoCheck,
        )
        .await.unwrap();

        self.node_conns.insert(1, client);
    }

    async fn exec(&self, cmd: &str) -> Result<CommandExecutedResult, async_ssh2_tokio::Error> {
        let mut chh;
        {
            chh = self.node_conns.get_mut(&1).unwrap().open_channel().await.unwrap();
        }
        
        return chh.execute(cmd).await;
    }

    async fn disconnect(&self){
        self.node_conns.remove(&1).unwrap().1.disconnect().await;
    }
}

#[tokio::main]
async fn main() -> Result<(), async_ssh2_tokio::Error> {
    
    let t = Arc::new(Tester::new());
    
    let mut tset = JoinSet::new();

    t.connect().await;

    let mut i = 0;
    while i < 4 {

        let c = t.clone();
        let cmdstr = format!("sleep {} && echo done {}", i, i);


        let fut = async move {
            let r = c.exec(cmdstr.as_str()).await.unwrap();
            println!("{}", r.output);
        };
        //Box::pin(fut);

        tset.spawn(fut);

        i += 1;
    }


    while let Some(res) = tset.join_next().await {
        let out = res.unwrap();
        println!("{:?}", out);
    }

    t.disconnect().await;

    Ok(())
}
