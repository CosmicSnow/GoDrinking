/** Master succession for Sala. Oldest remaining joinedAt, then id. */

export function nextMaster(members, leavingId) {
  const rest = members
    .filter((member) => member.id !== leavingId)
    .slice()
    .sort((left, right) => {
      if (left.joinedAt !== right.joinedAt) return left.joinedAt - right.joinedAt;
      return left.id < right.id ? -1 : left.id > right.id ? 1 : 0;
    });
  return rest[0] ? rest[0].id : null;
}
