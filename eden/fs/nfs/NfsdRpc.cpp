/*
 * Copyright (c) Facebook, Inc. and its affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

#ifndef _WIN32

#include "eden/fs/nfs/NfsdRpc.h"

namespace facebook::eden {
EDEN_XDR_SERDE_IMPL(specdata3, specdata1, specdata2);
EDEN_XDR_SERDE_IMPL(nfstime3, seconds, nseconds);
EDEN_XDR_SERDE_IMPL(
    fattr3,
    type,
    mode,
    nlink,
    uid,
    gid,
    size,
    used,
    rdev,
    fsid,
    fileid,
    atime,
    mtime,
    ctime);
EDEN_XDR_SERDE_IMPL(diropargs3, dir, name);
EDEN_XDR_SERDE_IMPL(GETATTR3resok, obj_attributes);
EDEN_XDR_SERDE_IMPL(LOOKUP3args, what);
EDEN_XDR_SERDE_IMPL(LOOKUP3resok, object, obj_attributes, dir_attributes);
EDEN_XDR_SERDE_IMPL(LOOKUP3resfail, dir_attributes);
EDEN_XDR_SERDE_IMPL(ACCESS3args, object, access);
EDEN_XDR_SERDE_IMPL(ACCESS3resok, obj_attributes, access);
EDEN_XDR_SERDE_IMPL(ACCESS3resfail, obj_attributes);
EDEN_XDR_SERDE_IMPL(
    FSINFO3resok,
    obj_attributes,
    rtmax,
    rtpref,
    rtmult,
    wtmax,
    wtpref,
    wtmult,
    dtpref,
    maxfilesize,
    time_delta,
    properties);
EDEN_XDR_SERDE_IMPL(FSINFO3resfail, obj_attributes);
EDEN_XDR_SERDE_IMPL(
    PATHCONF3resok,
    obj_attributes,
    linkmax,
    name_max,
    no_trunc,
    chown_restricted,
    case_insensitive,
    case_preserving);
EDEN_XDR_SERDE_IMPL(PATHCONF3resfail, obj_attributes);
} // namespace facebook::eden

#endif
