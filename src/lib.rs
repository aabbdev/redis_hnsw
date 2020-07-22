mod hnsw;
mod types;

#[macro_use]
extern crate redis_module;

#[macro_use]
extern crate lazy_static;

extern crate num;
extern crate ordered_float;
extern crate owning_ref;

use hnsw::{Index, Node};
use redis_module::{
    parse_float, parse_unsigned_integer, Context, RedisError, RedisResult, RedisValue,
};
use std::collections::{HashMap, HashSet};
use std::collections::hash_map::Entry;
use std::convert::TryInto;
use std::sync::{Arc, RwLock, RwLockWriteGuard};
use types::*;

static PREFIX: &str = "hnsw";

// type IndexArc = Arc<RwLock<Index<f32, f32>>>;
type IndexT = Index<f32, f32>;

lazy_static! {
    static ref INDICES: Arc<RwLock<HashMap<String, IndexT>>> =
        Arc::new(RwLock::new(HashMap::new()));
}

fn new_index(ctx: &Context, args: Vec<String>) -> RedisResult {
    if args.len() < 2 {
        return Err(RedisError::WrongArity);
    }

    ctx.auto_memory();

    let index_name = format!("{}.{}", PREFIX, &args[1]);

    let mut data_dim = 512;
    if args.len() > 2 {
        data_dim = parse_unsigned_integer(&args[2])?
            .try_into()
            .unwrap_or(data_dim);
    }
    let mut m = 5;
    if args.len() > 3 {
        m = parse_unsigned_integer(&args[3])?.try_into().unwrap_or(m);
    }
    let mut ef_construction = 200;
    if args.len() > 4 {
        ef_construction = parse_unsigned_integer(&args[4])?
            .try_into()
            .unwrap_or(ef_construction);
    }

    // write to redis
    let key = ctx.open_key_writable(&index_name);
    match key.get_value::<IndexRedis>(&HNSW_INDEX_REDIS_TYPE)? {
        Some(_) => {
            return Err(RedisError::String(format!(
                "Index: {} already exists",
                &index_name
            )));
        }
        None => {
            // create index
            let mut index = Index::new(
                &index_name,
                Box::new(hnsw::metrics::euclidean),
                data_dim,
                m,
                ef_construction,
            );
            ctx.log_debug(format!("{:?}", index).as_str());
            key.set_value::<IndexRedis>(&HNSW_INDEX_REDIS_TYPE, (& mut index).into())?;
            // Add index to global hashmap
            INDICES
                .write()
                .unwrap()
                .insert(index_name, index);
        }
    }

    Ok("OK".into())
}

fn get_index(ctx: &Context, args: Vec<String>) -> RedisResult {
    if args.len() < 2 {
        return Err(RedisError::WrongArity);
    }

    let mut indices = INDICES.write().unwrap();
    let index_name = format!("{}.{}", PREFIX, &args[1]);

    let index = load_index(ctx, & mut indices, &index_name)?;
    ctx.log_debug(format!("Index: {:?}", index).as_str());
    ctx.log_debug(format!("Layers: {:?}", index.layers.len()).as_str());
    ctx.log_debug(format!("Nodes: {:?}", index.nodes.len()).as_str());

    let index_redis: IndexRedis = index.into();

    Ok(index_redis.as_redisvalue())
}

fn delete_index(ctx: &Context, args: Vec<String>) -> RedisResult {
    if args.len() < 2 {
        return Err(RedisError::WrongArity);
    }
    {
        let index_name = format!("{}.{}", PREFIX, &args[1]);

        // get index from redis
        ctx.log_debug(format!("deleting index: {}", &index_name).as_str());
        let rkey = ctx.open_key_writable(&index_name);

        match rkey.get_value::<IndexRedis>(&HNSW_INDEX_REDIS_TYPE)? {
            Some(_) => rkey.delete()?,
            None => {
                return Err(RedisError::String(format!(
                    "Index: {} does not exist",
                    &args[1]
                )));
            }
        };

        // get index from global hashmap
        let mut indices = INDICES.write().unwrap();
        indices
            .remove(&index_name)
            .ok_or_else(|| format!("Index: {} does not exist", &args[1]))?;
    }
    Ok(1_usize.into())
}

fn load_index<'a>(ctx: &Context, indices: &'a mut RwLockWriteGuard<HashMap<String, IndexT>>, index_name: &str) -> Result<&'a mut IndexT, RedisError> {
    // check if index is in global hashmap
    let index = match indices.entry(index_name.to_string()) {
        Entry::Occupied(o) => o.into_mut(),
        // if index isn't present, load it from redis
        Entry::Vacant(v) => {
            // get index from redis
            ctx.log_debug(format!("get key: {}", &index_name).as_str());
            let rkey = ctx.open_key(&index_name);

            let index_redis = match rkey.get_value::<IndexRedis>(&HNSW_INDEX_REDIS_TYPE)? {
                Some(index) => index,
                None => return Err(format!("Index: {} does not exist", index_name).into()),
            };
            let index = make_index(ctx, index_redis)?;
            v.insert(index)
        }
    };

    Ok(index)
}

fn make_index(ctx: &Context, ir: &IndexRedis) -> Result<IndexT, RedisError> {
    let mut index: IndexT = ir.into();

    index.nodes = HashMap::with_capacity(ir.node_count);
    for node_name in &ir.nodes {
        let key = ctx.open_key(&node_name);

        let nr = match key.get_value::<NodeRedis>(&HNSW_NODE_REDIS_TYPE)? {
            Some(n) => n,
            None => return Err(format!("Node: {} does not exist", node_name).into()),
        };
        let node = Node::new(node_name, &nr.data, index.m_max_0);
        index.nodes.insert(node_name.to_owned(), node);
    }

    // reconstruct nodes
    for node_name in &ir.nodes {
        let target = index.nodes.get(node_name).unwrap();

        let key = ctx.open_key(&node_name);

        let nr = match key.get_value::<NodeRedis>(&HNSW_NODE_REDIS_TYPE)? {
            Some(n) => n,
            None => return Err(format!("Node: {} does not exist", node_name).into()),
        };
        for layer in &nr.neighbors {
            let mut node_layer = Vec::with_capacity(layer.len());
            for neighbor in layer {
                let nn = match index.nodes.get(neighbor) {
                    Some(node) => node,
                    None => return Err(format!("Node: {} does not exist", node_name).into()),
                };
                node_layer.push(nn.downgrade());
            }
            target.write().neighbors.push(node_layer);
        }
    }

    // reconstruct layers
    for layer in &ir.layers {
        let mut node_layer = HashSet::with_capacity(layer.len());
        for node_name in layer {
            let node = match index.nodes.get(node_name) {
                Some(n) => n,
                None => return Err(format!("Node: {} does not exist", node_name).into()),
            };
            node_layer.insert(node.downgrade());
        }
        index.layers.push(node_layer);
    }

    // set enterpoint
    index.enterpoint = match &ir.enterpoint {
        Some(node_name) => {
            let node = match index.nodes.get(node_name) {
                Some(n) => n,
                None => return Err(format!("Node: {} does not exist", node_name).into()),
            };
            Some(node.downgrade())
        }
        None => None,
    };

    Ok(index)
}

fn update_index(
    ctx: &Context,
    index_name: &str,
    index: & mut IndexT,
) -> Result<(), RedisError> {
    let key = ctx.open_key_writable(index_name);
    match key.get_value::<IndexRedis>(&HNSW_INDEX_REDIS_TYPE)? {
        Some(_) => {
            ctx.log_debug(format!("update index: {}", index_name).as_str());
            key.set_value::<IndexRedis>(&HNSW_INDEX_REDIS_TYPE, index.into())?;
        }
        None => {
            return Err(RedisError::String(format!(
                "Index: {} does not exist",
                index_name
            )));
        }
    }
    Ok(())
}

fn add_node(ctx: &Context, args: Vec<String>) -> RedisResult {
    if args.len() < 4 {
        return Err(RedisError::WrongArity);
    }

    ctx.auto_memory();

    let index_name = format!("{}.{}", PREFIX, &args[1]);
    let node_name = format!("{}.{}.{}", PREFIX, &args[1], &args[2]);

    let dataf64 = &args[3..]
        .iter()
        .map(|s| parse_float(s))
        .collect::<Result<Vec<f64>, RedisError>>()?;
    let data = dataf64.iter().map(|d| *d as f32).collect::<Vec<f32>>();

    let mut indices = INDICES.write().unwrap();
    let index = load_index(ctx, & mut indices, &index_name)?;

    ctx.log_debug(format!("Adding node: {} to Index: {}", &node_name, &index_name).as_str());
    if let Err(e) = index.add_node(&node_name, &data, update_node) {
        return Err(e.error_string().into())
    }

    // write node to redis
    let node = index.nodes.get(&node_name).unwrap();
    write_node(ctx, &node_name, node.into())?;

    // update index in redis
    update_index(ctx, &index_name, & mut *index)?;

    Ok("OK".into())
}

fn delete_node(ctx: &Context, args: Vec<String>) -> RedisResult {
    if args.len() < 3 {
        return Err(RedisError::WrongArity);
    }

    let mut indices = INDICES.write().unwrap();
    let index_name = format!("{}.{}", PREFIX, &args[1]);
    let index = load_index(ctx, & mut indices, &index_name)?;

    let node_name = format!("{}.{}.{}", PREFIX, &args[1], &args[2]);

    // TODO return error if node has more than 1 strong_count
    let node = index.nodes.get(&node_name).unwrap();
    if Arc::strong_count(&node.0) > 1 {
        return Err(format!(
            "{} is being accessed, unable to delete. Try again later",
            &node_name
        )
        .into());
    }
    if let Err(e) = index.delete_node(&node_name, update_node) {
        return Err(e.error_string().into())
    }

    ctx.log_debug(format!("del key: {}", &node_name).as_str());
    let rkey = ctx.open_key_writable(&node_name);
    match rkey.get_value::<NodeRedis>(&HNSW_NODE_REDIS_TYPE)? {
        Some(_) => rkey.delete()?,
        None => {
            return Err(RedisError::String(format!(
                "Node: {} does not exist",
                &node_name
            )));
        }
    };

    // update index in redis
    update_index(ctx, &index_name, & mut *index)?;

    Ok(1_usize.into())
}

fn get_node(ctx: &Context, args: Vec<String>) -> RedisResult {
    if args.len() < 3 {
        return Err(RedisError::WrongArity);
    }

    let node_name = format!("{}.{}.{}", PREFIX, &args[1], &args[2]);

    ctx.log_debug(format!("get key: {}", node_name).as_str());

    let key = ctx.open_key(&node_name);

    let value = match key.get_value::<NodeRedis>(&HNSW_NODE_REDIS_TYPE)? {
        Some(node) => node.as_redisvalue(),
        None => {
            return Err(RedisError::String(format!(
                "Node: {} does not exist",
                &node_name
            )));
        }
    };

    Ok(value)
}

fn write_node<'a>(ctx: &'a Context, key: &str, node: NodeRedis) -> RedisResult {
    ctx.log_debug(format!("set key: {}", key).as_str());
    let rkey = ctx.open_key_writable(key);

    match rkey.get_value::<NodeRedis>(&HNSW_NODE_REDIS_TYPE)? {
        Some(value) => {
            value.data = node.data;
            value.neighbors = node.neighbors;
        }
        None => {
            rkey.set_value(&HNSW_NODE_REDIS_TYPE, node)?;
        }
    }
    Ok(key.into())
}

fn update_node(name: String, node: hnsw::Node<f32>) {
    let ctx = Context::get_thread_safe_context();
    ctx.lock();
    write_node(&ctx, &name, (&node).into()).unwrap();
    ctx.unlock();
}

fn search_knn(ctx: &Context, args: Vec<String>) -> RedisResult {
    if args.len() < 4 {
        return Err(RedisError::WrongArity);
    }

    let index_name = format!("{}.{}", PREFIX, &args[1]);
    let k = parse_unsigned_integer(&args[2])? as usize;
    let dataf64 = &args[3..]
        .iter()
        .map(|s| parse_float(s))
        .collect::<Result<Vec<f64>, RedisError>>()?;
    let data = dataf64.iter().map(|d| *d as f32).collect::<Vec<f32>>();

    let mut indices = INDICES.write().unwrap();
    let index = load_index(ctx, & mut indices, &index_name)?;

    ctx.log_debug(
        format!(
            "Searching for {} nearest nodes in Index: {}",
            k, &index_name
        )
        .as_str(),
    );

    match index.search_knn(&data, k) {
        Ok(res) => {
            {
                let mut reply: Vec<RedisValue> = Vec::new();
                reply.push(res.len().into());
                for r in &res {
                    let sr: SearchResultRedis = r.into();
                    reply.push(sr.as_redisvalue());
                }
                Ok(reply.into())
            }
        }
        Err(e) => Err(e.error_string().into()),
    }
}

redis_module! {
    name: "hnsw",
    version: 1,
    data_types: [
        HNSW_INDEX_REDIS_TYPE,
        HNSW_NODE_REDIS_TYPE,
    ],
    commands: [
        ["hnsw.new", new_index, "write"],
        ["hnsw.get", get_index, "readonly"],
        ["hnsw.del", delete_index, "write"],
        ["hnsw.search", search_knn, "readonly"],
        ["hnsw.node.add", add_node, "write"],
        ["hnsw.node.get", get_node, "readonly"],
        ["hnsw.node.del", delete_node, "write"],
    ],
}
