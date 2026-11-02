import osmium
import pytest


@pytest.fixture
def pbf(tmp_path):
    """Generate real binary PBF fixtures with independently specified OSM objects."""
    counter = 0

    def write(
        nodes, ways, *, timestamp="2026-01-02T03:04:05Z", relations=(), node_tags=None, options=""
    ):
        nonlocal counter
        counter += 1
        path = tmp_path / f"extract-{counter}.osm.pbf"
        header = osmium.io.Header()
        if timestamp:
            header.set("osmosis_replication_timestamp", timestamp)
        target = osmium.io.File(str(path), f"pbf{options}") if options else str(path)
        with osmium.SimpleWriter(target, header=header) as writer:
            for node_id, lonlat in sorted(nodes.items()):
                tags = (node_tags or {}).get(node_id, {})
                writer.add_node(osmium.osm.mutable.Node(id=node_id, location=lonlat, tags=tags))
            for way_id, refs, tags in sorted(ways):
                writer.add_way(osmium.osm.mutable.Way(id=way_id, nodes=refs, tags=tags))
            for relation in relations:
                writer.add_relation(relation)
        return path

    return write
