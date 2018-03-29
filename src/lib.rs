extern crate argon2rs;
extern crate rand;
extern crate syscall;
#[macro_use]
extern crate failure;

use std::convert::From;
use std::fmt::{self, Display};
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::iter::FromIterator;
#[cfg(target_os = "redox")]
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::slice::{Iter, IterMut};
use std::str::FromStr;
use std::thread::sleep;
use std::time::Duration;

use argon2rs::verifier::Encoded;
use argon2rs::{Argon2, Variant};
use failure::Error;
use rand::Rng;
use rand::os::OsRng;
use syscall::Error as SyscallError;
#[cfg(target_os = "redox")]
use syscall::flag::{O_EXLOCK, O_SHLOCK};

//TODO: Allow a configuration file for all this someplace
#[cfg(not(test))]
const PASSWD_FILE: &'static str = "/etc/passwd";
#[cfg(not(test))]
const GROUP_FILE: &'static str = "/etc/group";
const MIN_GID: usize = 1000;
const MAX_GID: usize = 6000;
const MIN_UID: usize = 1000;
const MAX_UID: usize = 6000;
const TIMEOUT: u64 = 3;

// Testing values
#[cfg(test)]
const PASSWD_FILE: &'static str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/passwd");
#[cfg(test)]
const GROUP_FILE: &'static str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/group");

pub type Result<T> = std::result::Result<T, Error>;

/// Errors that might happen while using this crate
#[derive(Debug, Fail, PartialEq)]
pub enum UsersError {
    #[fail(display = "os error: code {}", reason)]
    Os { reason: String },
    #[fail(display = "parse error: {}", reason)]
    Parsing { reason: String },
    #[fail(display = "user/group not found")]
    NotFound,
    #[fail(display = "user/group already exists")]
    AlreadyExists
}

fn parse_error(reason: &str) -> UsersError {
    UsersError::Parsing { reason: reason.into() }
}

fn os_error(reason: &str) -> UsersError {
    UsersError::Os { reason: reason.into() }
}

impl From<SyscallError> for UsersError {
    fn from(syscall_error: SyscallError) -> UsersError {
        UsersError::Os { reason: format!("{}", syscall_error) }
    }
}

fn read_locked_file(file: &str) -> Result<String> {
    #[cfg(test)]
    println!("Reading file: {}", file);

    #[cfg(target_os = "redox")]
    #[cfg_attr(rustfmt, rustfmt_skip)]
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(O_SHLOCK as i32)
        .open(file)?;
    #[cfg(not(target_os = "redox"))]
    #[cfg_attr(rustfmt, rustfmt_skip)]
    let mut file = OpenOptions::new()
        .read(true)
        .open(file)?;

    let len = file.metadata()?.len();
    let mut file_data = String::with_capacity(len as usize);
    file.read_to_string(&mut file_data)?;
    Ok(file_data)
}

fn write_locked_file(file: &str, data: String) -> Result<()> {
    #[cfg(test)]
    println!("Reading file: {}", file);

    #[cfg(target_os = "redox")]
    #[cfg_attr(rustfmt, rustfmt_skip)]
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .custom_flags(O_EXLOCK as i32)
        .open(file)?;
    #[cfg(not(target_os = "redox"))]
    #[cfg_attr(rustfmt, rustfmt_skip)]
    let mut file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(file)?;

    file.write(data.as_bytes())?;
    Ok(())
}

/// A struct representing a Redox user.
/// Currently maps to an entry in the '/etc/passwd' file.
///
/// # Unset vs. Blank Passwords
/// A note on unset passwords vs. blank passwords. A blank password
/// is a hash field that is completely blank (aka, `""`). According
/// to this crate, login is only allowed if the input password is blank
/// as well.
///
/// An unset
/// password is one whose hash is not empty (`""`), but also not a valid
/// serialized argon2rs hashing session. This hash always returns false
/// upon attempted verification. The most commonly used hash for an
/// unset password is `"!"`, but this crate makes no distinction.
/// The most common way to unset the password is to use [`unset_passwd`](struct.User.html#method.unset_passwd).
///
/// Note that `"x"` should not be used as the hash field because
/// that indicates that a hash is stored in the shadowfile, not passwd
pub struct User {
    /// Username (login name)
    pub user: String,
    /// Hashed password
    hash: String,
    /// Argon2 Hashing session, stored to simplify API
    encoded: Option<Encoded>,
    /// User id
    pub uid: usize,
    /// Group id
    pub gid: usize,
    /// Real name (GECOS field)
    pub name: String,
    /// Home directory path
    pub home: String,
    /// Shell path
    pub shell: String
}

impl User {
    /// Set the password for a user. Make sure the password you have
    /// received is actually what the user wants as their password.
    ///
    /// To set the password blank, use `""` as the password parameter.
    pub fn set_passwd(&mut self, password: &str) -> Result<()> {
        let hash;
        let encoded;
        if password != "" {
            let a2 = Argon2::new(10, 1, 4096, Variant::Argon2i)?;
            let salt = format!("{:X}", OsRng::new()?.next_u64());
            let enc = Encoded::new(a2, password.as_bytes(), salt.as_bytes(), &[], &[]);

            hash = String::from_utf8(enc.to_u8())?;
            encoded = Some(enc);
        } else {
            hash = "".into();
            encoded = None;
        }

        self.hash = hash;
        self.encoded = encoded;
        Ok(())
    }

    /// Unset the password (do not allow logins)
    pub fn unset_passwd(&mut self) {
        self.encoded = None;
        self.hash = "!".into();
    }

    /// Verify the password. If the hash is empty, we only
    /// allow login if the password field is also empty.
    /// Note that this is a blocking operation if the password
    /// is incorrect.
    pub fn verify_passwd(&self, password: &str) -> bool {
        let verified = if let Some(ref encoded) = self.encoded {
            encoded.verify(password.as_bytes())
        } else {
            self.hash == "" && password == ""
        };

        if !verified {
            sleep(Duration::new(TIMEOUT, 0));
        }
        verified
    }

    /// Determine if the hash for the password is blank
    /// (Any user can log in as this user with no password).
    pub fn is_passwd_blank(&self) -> bool {
        self.hash == ""
    }

    /// Determine if the hash for the password is unset
    /// (No users can log in as this user, aka, must use sudo or su)
    pub fn is_passwd_unset(&self) -> bool {
        self.encoded.is_none() && self.hash != ""
    }

    /// Get a Command to run the user's default shell
    /// (See [`login_cmd`](struct.User.html#method.login_cmd) for more doc)
    pub fn shell_cmd(&self) -> Command {
        self.login_cmd(&self.shell)
    }

    /// Provide a login command for the user, which is any
    /// entry point for starting a user's session, whether
    /// a shell (use [`shell_cmd`](struct.User.html#method.shell_cmd) instead) or a graphical init.
    ///
    /// The `Command` will have set the users UID and GID, its CWD will be
    /// set to the users's home directory and the follwing enviroment variables will
    /// be populated like so:
    ///
    ///    - `USER` set to the user's `user` field.
    ///    - `UID` set to the user's `uid` field.
    ///    - `GROUPS` set the user's `gid` field.
    ///    - `HOME` set to the user's `home` field.
    ///    - `SHELL` set to the user's `shell` field.
    pub fn login_cmd<T: AsRef<str>>(&self, cmd: T) -> Command
        where T: std::convert::AsRef<std::ffi::OsStr>
    {
        let mut command = Command::new(cmd);
        command.uid(self.uid as u32)
               .gid(self.gid as u32)
               .current_dir(&self.home)
               .env("USER", &self.user)
               .env("UID", format!("{}", self.uid))
               .env("GROUPS", format!("{}", self.gid))
               .env("HOME", &self.home)
               .env("SHELL", &self.shell);
        command
    }
}

impl Display for User {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        #[cfg_attr(rustfmt, rustfmt_skip)]
        write!(f, "{};{};{};{};{};{};{}",
            self.user, self.hash, self.uid, self.gid, self.name, self.home, self.shell
        )
    }
}

impl FromStr for User {
    type Err = failure::Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut parts = s.split(';');

        let user = parts.next().ok_or(parse_error("expected user"))?;
        let hash = parts.next().ok_or(parse_error("expected hash"))?;
        let uid = parts.next()
                       .ok_or(parse_error("expected uid"))?
                       .parse::<usize>()?;
        let gid = parts.next()
                       .ok_or(parse_error("expected uid"))?
                       .parse::<usize>()?;
        let name = parts.next().ok_or(parse_error("expected real name"))?;
        let home = parts.next().ok_or(parse_error("expected home dir path"))?;
        let shell = parts.next().ok_or(parse_error("expected shell path"))?;

        let encoded = match hash {
            "" => None,
            "!" => None,
            _ => Some(Encoded::from_u8(hash.as_bytes())?)
        };

        Ok(User { user: user.into(),
                  hash: hash.into(),
                  encoded: encoded,
                  uid: uid,
                  gid: gid,
                  name: name.into(),
                  home: home.into(),
                  shell: shell.into() })
    }
}

/// A struct representing a Redox users group.
/// Currently maps to an '/etc/group' file entry.
pub struct Group {
    /// Group name
    pub group: String,
    /// Unique group id
    pub gid: usize,
    /// Group members usernames
    pub users: Vec<String>
}

impl Display for Group {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        #[cfg_attr(rustfmt, rustfmt_skip)]
        write!(f, "{};{};{}",
            self.group,
            self.gid,
            self.users.join(",").trim_matches(',')
        )
    }
}

impl FromStr for Group {
    type Err = failure::Error;

    fn from_str(s: &str) -> Result<Self> {
        let mut parts = s.split(';');

        let group = parts.next().ok_or(parse_error("expected group"))?;
        let gid = parts.next()
                       .ok_or(parse_error("expected gid"))?
                       .parse::<usize>()?;
        //Allow for an empty users field. If there is a better way to do this, do it
        let users_str = parts.next().unwrap_or(" ");
        let users = users_str.split(',').map(|u| u.into()).collect();

        Ok(Group { group: group.into(),
                   gid: gid,
                   users: users })
    }
}

/// Gets the current process effective user ID.
///
/// This function issues the `geteuid` system call returning the process effective
/// user id.
///
/// # Examples
///
/// Basic usage:
///
/// ```
/// # use redox_users::get_euid;
/// let euid = get_euid().unwrap();
/// ```
pub fn get_euid() -> Result<usize> {
    match syscall::geteuid() {
        Ok(euid) => Ok(euid),
        Err(syscall_error) => Err(From::from(os_error(syscall_error.text())))
    }
}

/// Gets the current process real user ID.
///
/// This function issues the `getuid` system call returning the process real
/// user id.
///
/// # Examples
///
/// Basic usage:
///
/// ```
/// # use redox_users::get_uid;
/// let uid = get_uid().unwrap();
/// ```
pub fn get_uid() -> Result<usize> {
    match syscall::getuid() {
        Ok(uid) => Ok(uid),
        Err(syscall_error) => Err(From::from(os_error(syscall_error.text())))
    }
}

/// Gets the current process effective group ID.
///
/// This function issues the `getegid` system call returning the process effective
/// group id.
///
/// # Examples
///
/// Basic usage:
///
/// ```
/// # use redox_users::get_egid;
/// let egid = get_egid().unwrap();
/// ```
pub fn get_egid() -> Result<usize> {
    match syscall::getegid() {
        Ok(egid) => Ok(egid),
        Err(syscall_error) => Err(From::from(os_error(syscall_error.text())))
    }
}

/// Gets the current process real group ID.
///
/// This function issues the `getegid` system call returning the process real
/// group id.
///
/// # Examples
///
/// Basic usage:
///
/// ```
/// # use redox_users::get_gid;
/// let gid = get_gid().unwrap();
/// ```
pub fn get_gid() -> Result<usize> {
    match syscall::getgid() {
        Ok(gid) => Ok(gid),
        Err(syscall_error) => Err(From::from(os_error(syscall_error.text())))
    }
}

/// Struct encapsulating all users on the system
///
/// [`AllUsers`](struct.AllUsers.html) is a struct providing
/// (borrowed) access to all the users and groups on the system.
///
/// ## Notes
/// Note that everything in this section also applies to
/// [`AllGroups`](struct.AllGroups.html)
///
/// * If you mutate anything in conjunction with an AllUsers,
///   you must call the [`save`](struct.AllUsers.html#method.save)
///   method in order for those changes to be applied to the system.
/// * The API here is kept small on purpose in order to reduce the
///   surface area for security exploitation. Most mutating actions
///   can be accomplished via the [`get_mut_by_id`](struct.AllUsers.html#method.get_mut_by_id)
///   and [`get_mut_by_name`](struct.AllUsers.html#method.get_mut_by_name)
///   functions.
pub struct AllUsers {
    users: Vec<User>
}

impl AllUsers {
    /// Create a new AllUsers
    //TODO: Indicate if parsing an individual line failed or not
    pub fn new() -> Result<AllUsers> {
        let passwd_cntnt = read_locked_file(PASSWD_FILE)?;

        let mut entries: Vec<User> = Vec::new();

        for line in passwd_cntnt.lines() {
            if let Ok(user) = User::from_str(line) {
                entries.push(user);
            }
        }

        Ok(AllUsers { users: entries })
    }

    /// Get an iterator over all system groups
    pub fn iter(&self) -> Iter<User> {
        self.users.iter()
    }

    /// Mutable version of `iter`
    pub fn iter_mut(&mut self) -> IterMut<User> {
        self.users.iter_mut()
    }

    /// Borrow the [`User`](struct.User.html) representing a user for a given username.
    ///
    /// # Examples
    ///
    /// Basic usage:
    ///
    /// ```no_run
    /// # use redox_users::AllUsers;
    /// let users = AllUsers::new().unwrap();
    /// let user = users.get_by_id(0).unwrap();
    /// ```
    pub fn get_by_name<T: AsRef<str>>(&self, username: T) -> Option<&User> {
        self.iter().find(|user| user.user == username.as_ref())
    }

    /// Mutable version of ['get_by_name'](struct.AllUsers.html#method.get_by_name)
    pub fn get_mut_by_name<T: AsRef<str>>(&mut self, username: T) -> Option<&mut User> {
        self.iter_mut().find(|user| user.user == username.as_ref())
    }

    /// Borrow the [`User`](struct.AllUsers.html) representing given user ID.
    ///
    /// # Examples
    ///
    /// Basic usage:
    ///
    /// ```no_run
    /// # use redox_users::AllUsers;
    /// let users = AllUsers::new().unwrap();
    /// let user = users.get_by_id(0).unwrap();
    /// ```
    pub fn get_by_id(&self, uid: usize) -> Option<&User> {
        self.iter().find(|user| user.uid == uid)
    }

    /// Mutable version of [`get_by_id`](struct.AllUsers.html#method.get_by_id)
    pub fn get_mut_by_id(&mut self, uid: usize) -> Option<&mut User> {
        self.iter_mut().find(|user| user.uid == uid)
    }

    /// Provides an unused user id, defined as "unused" by the system
    /// defaults, between 1000 and 6000
    ///
    /// # Examples
    ///
    /// ```
    /// # use redox_users::AllUsers;
    /// let users = AllUsers::new().unwrap();
    /// let uid = users.get_unique_id().expect("no available uid");
    /// ```
    pub fn get_unique_id(&self) -> Option<usize> {
        for uid in MIN_UID..MAX_UID {
            if !self.iter().any(|user| uid == user.uid) {
                return Some(uid);
            }
        }
        None
    }

    /// Adds a user with the specified attributes to the
    /// AllUsers instance. Note that the
    /// user's password is set unset during this call.
    ///
    /// This function is classified as a mutating operation,
    /// and users must therefore call [`save`](struct.AllUsers.html#method.save)
    /// in order for the new user to be applied to the system.
    pub fn add_user(&mut self,
                    login: &str,
                    uid: usize,
                    gid: usize,
                    name: &str,
                    home: &str,
                    shell: &str)
                    -> Result<()> {
        if self.iter().any(|user| user.user == login || user.uid == uid) {
            return Err(From::from(UsersError::AlreadyExists));
        }

        self.users.push(User { user: login.into(),
                               hash: "!".into(),
                               encoded: None,
                               uid: uid,
                               gid: gid,
                               name: name.into(),
                               home: home.into(),
                               shell: shell.into() });
        Ok(())
    }

    /// Remove a user from the system. This is a mutating operation,
    /// and users of the crate must therefore call [`save`](struct.AllUsers.html#method.save)
    /// in order for changes to be applied to the system.
    pub fn remove_by_name(&mut self, name: String) -> Result<()> {
        self.remove(|user| user.user == name)
    }

    /// User-id version of [`remove_by_name`](struct.AllUsers.html#method.remove_by_name)
    pub fn remove_by_id(&mut self, id: usize) -> Result<()> {
        self.remove(|user| user.uid == id)
    }

    // Reduce code duplication
    fn remove<P>(&mut self, predicate: P) -> Result<()>
        where P: FnMut(&User) -> bool
    {
        let pos;
        {
            let mut iter = self.iter();
            if let Some(posi) = iter.position(predicate) {
                pos = posi;
            } else {
                return Err(From::from(UsersError::NotFound));
            };
        }

        self.users.remove(pos);

        Ok(())
    }

    /// Syncs the data stored in the AllUsers instance to the filesystem.
    /// To apply changes to the system from an AllUsers, you MUST call this function!
    /// This function currently does a bunch of fs I/O so it is error-prone.
    pub fn save(&self) -> Result<()> {
        let mut userstring = String::new();
        for user in &self.users {
            userstring.push_str(&format!("{}\n", user.to_string().as_str()));
        }

        write_locked_file(PASSWD_FILE, userstring)
    }
}

impl FromIterator<User> for AllUsers {
    fn from_iter<I: IntoIterator<Item = User>>(iter: I) -> Self {
        let mut entries = Vec::new();

        for user in iter {
            entries.push(user)
        }

        AllUsers { users: entries }
    }
}

impl IntoIterator for AllUsers {
    type Item = User;
    type IntoIter = ::std::vec::IntoIter<User>;

    fn into_iter(self) -> Self::IntoIter {
        self.users.into_iter()
    }
}

impl<'a> IntoIterator for &'a AllUsers {
    type Item = &'a User;
    type IntoIter = ::std::slice::Iter<'a, User>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut AllUsers {
    type Item = &'a mut User;
    type IntoIter = ::std::slice::IterMut<'a, User>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

/// Struct encapsulating all the groups on the system
///
/// [`AllGroups`](struct.AllGroups.html) is a struct that provides
/// (borrowed) access to all groups on the system.
///
/// General notes that also apply to this struct may be found with
/// [`AllUsers`](struct.AllUsers.html).
pub struct AllGroups {
    groups: Vec<Group>
}

impl IntoIterator for AllGroups {
    type Item = Group;
    type IntoIter = ::std::vec::IntoIter<Group>;

    fn into_iter(self) -> Self::IntoIter {
        self.groups.into_iter()
    }
}

impl<'a> IntoIterator for &'a AllGroups {
    type Item = &'a Group;
    type IntoIter = ::std::slice::Iter<'a, Group>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut AllGroups {
    type Item = &'a mut Group;
    type IntoIter = ::std::slice::IterMut<'a, Group>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

//UNOPTIMIZED: Right now this struct is just a Vec and we are doing O(n)
// operations over the vec to do the `get` methods. A multi-key
// hashmap would be a godsend here for performance.
impl AllGroups {
    /// Create a new AllGroups
    //TODO: Indicate if parsing an individual line failed or not
    pub fn new() -> Result<AllGroups> {
        let group_cntnt = read_locked_file(GROUP_FILE)?;

        let mut entries: Vec<Group> = Vec::new();

        for line in group_cntnt.lines() {
            if let Ok(group) = Group::from_str(line) {
                entries.push(group);
            }
        }

        Ok(AllGroups { groups: entries })
    }

    /// Get an iterator over all system groups
    pub fn iter(&self) -> Iter<Group> {
        self.groups.iter()
    }

    /// Mutable version of `iter`
    pub fn iter_mut(&mut self) -> IterMut<Group> {
        self.groups.iter_mut()
    }

    /// Gets the [`Group`](struct.Group.html) for a given group name.
    ///
    /// # Examples
    ///
    /// Basic usage:
    ///
    /// ```no_run
    /// # use redox_users::AllGroups;
    /// let groups = AllGroups::new().unwrap();
    /// let group = groups.get_by_name("wheel").unwrap();
    /// ```
    pub fn get_by_name<T: AsRef<str>>(&self, groupname: T) -> Option<&Group> {
        self.iter().find(|group| group.group == groupname.as_ref())
    }

    /// Mutable version of [`get_by_name`](struct.AllGroups.html#method.get_by_name)
    pub fn get_mut_by_name<T: AsRef<str>>(&mut self, groupname: T) -> Option<&mut Group> {
        self.iter_mut().find(|group| group.group == groupname.as_ref())
    }

    /// Gets the [`Group`](struct.Group.html) for a given group ID.
    ///
    /// # Examples
    ///
    /// Basic usage:
    ///
    /// ```no_run
    /// # use redox_users::AllGroups;
    /// let groups = AllGroups::new().unwrap();
    /// let group = groups.get_by_id(1).unwrap();
    /// ```
    pub fn get_by_id(&self, gid: usize) -> Option<&Group> {
        self.iter().find(|group| group.gid == gid)
    }

    /// Mutable version of [`get_by_id`](struct.AllGroups.html#method.get_by_id)
    pub fn get_mut_by_id(&mut self, gid: usize) -> Option<&mut Group> {
        self.iter_mut().find(|group| group.gid == gid)
    }

    /// Provides an unused group id, defined as "unused" by the system
    /// defaults, between 1000 and 6000
    ///
    /// # Examples
    ///
    /// ```
    /// # use redox_users::AllGroups;
    /// let groups = AllGroups::new().unwrap();
    /// let gid = groups.get_unique_id().expect("no available gid");
    /// ```
    pub fn get_unique_id(&self) -> Option<usize> {
        for gid in MIN_GID..MAX_GID {
            if !self.iter().any(|group| gid == group.gid) {
                return Some(gid);
            }
        }
        None
    }

    /// Adds a group with the specified attributes to this AllGroups
    ///
    /// This function is classified as a mutating operation,
    /// and users must therefore call [`save`](struct.AllUsers.html#method.save)
    /// in order for the new group to be applied to the system.
    pub fn add_group(&mut self, name: &str, gid: usize, users: &[&str]) -> Result<()> {
        if self.iter().any(|group| group.group == name || group.gid == gid) {
            return Err(From::from(UsersError::AlreadyExists));
        }

        self.groups.push(Group {
            group: name.into(),
            gid: gid,
            users: users.iter().map(|user| user.to_string()).collect()
            //Might be cleaner... Also breaks...
            //users: users.iter().map(String::to_string).collect()
        });

        Ok(())
    }

    /// Remove a group from the system. This is a mutating operation,
    /// and users of the crate must therefore call [`save`](struct.AllGroups.html#method.save)
    /// in order for changes to be applied to the system.
    pub fn remove_by_name(&mut self, name: String) -> Result<()> {
        self.remove(|group| group.group == name)
    }

    /// Group-id version of [`remove_by_name`](struct.AllGroups.html#method.remove_by_name)
    pub fn remove_by_id(&mut self, id: usize) -> Result<()> {
        self.remove(|group| group.gid == id)
    }

    // Reduce code duplication
    fn remove<P>(&mut self, predicate: P) -> Result<()>
        where P: FnMut(&Group) -> bool
    {
        let pos;
        {
            let mut iter = self.iter();
            if let Some(posi) = iter.position(predicate) {
                pos = posi;
            } else {
                return Err(From::from(UsersError::NotFound));
            };
        }

        self.groups.remove(pos);

        Ok(())
    }

    /// Syncs the data stored in the AllGroups instance to the filesystem.
    /// To apply changes to the AllGroups, you MUST call this function.
    /// This function currently does a lot of fs I/O so it is error-prone.
    pub fn save(&self) -> Result<()> {
        let mut groupstring = String::new();
        for group in &self.groups {
            groupstring.push_str(&format!("{}\n", group.to_string().as_str()));
        }

        write_locked_file(GROUP_FILE, groupstring)
    }
}

#[cfg(test)]
#[cfg_attr(rustfmt, rustfmt_skip)]
mod test {
    use super::*;

    fn get_test_all_users() -> AllUsers {
        AllUsers::new().unwrap_or_else(|err| {
            println!("{}", err);
            panic!(err);
        })
    }

    #[test]
    fn get_user() {
        let users = get_test_all_users();
        let root = users.get_by_id(0).unwrap();
        assert_eq!(root.user,  "root".to_string());
        assert_eq!(root.hash,
            "$argon2i$m=4096,t=10,p=1$Tnc4UVV0N00$ML9LIOujd3nmAfkAwEcSTMPqakWUF0OUiLWrIy0nGLk".to_string());
        assert_eq!(root.uid,   0);
        assert_eq!(root.gid,   0);
        assert_eq!(root.name,  "root".to_string());
        assert_eq!(root.home,  "file:/root".to_string());
        assert_eq!(root.shell, "file:/bin/ion".to_string());
        match root.encoded {
            Some(_) => (),
            None => panic!("Expected encoded argon hash!")
        }

        let user = users.get_by_name("user").unwrap();
        assert_eq!(user.user,  "user".to_string());
        assert_eq!(user.hash,  "".to_string());
        assert_eq!(user.uid,   1000);
        assert_eq!(user.gid,   1000);
        assert_eq!(user.name,  "user".to_string());
        assert_eq!(user.home,  "file:/home/user".to_string());
        assert_eq!(user.shell, "file:/bin/ion".to_string());
        match user.encoded {
            Some(_) => panic!("Should not be an argon hash!"),
            None => ()
        }
    }

    fn get_test_all_groups() -> AllGroups {
        AllGroups::new().unwrap_or_else(|err| {
            println!("{}", err);
            panic!(err);
        })
    }

    #[test]
    fn get_group() {
        let groups = get_test_all_groups();
        let user = groups.get_by_name("user").unwrap();
        assert_eq!(user.group, "user");
        assert_eq!(user.gid,   1000);
        assert_eq!(user.users, vec!["user"]);

        let wheel = groups.get_by_id(1).unwrap();
        assert_eq!(wheel.group, "wheel");
        assert_eq!(wheel.gid,   1);
        assert_eq!(wheel.users, vec!["user", "root"]);
    }

    #[test]
    fn get_unused_ids() {
        let users = get_test_all_users();
        let id = users.get_unique_id().unwrap();
        if id < MIN_UID || id > MAX_UID {
            panic!("User ID is not between allowed margins")
        } else if let Some(_) = users.get_by_id(id) {
            panic!("User ID is used!");
        }

        let groups = get_test_all_groups();
        let id = groups.get_unique_id().unwrap();
        if id < MIN_GID || id > MAX_GID {
            panic!("Group ID is not between allowed margins")
        } else if let Some(_) = groups.get_by_id(id) {
            panic!("Group ID is used!");
        }
    }

    #[test]
    fn manip_user() {
        let mut users = get_test_all_users();
        // NOT testing `get_unique_id`
        let id = 7099;
        users.add_user("fb", id, id, "FooBar", "/home/foob", "/bin/zsh").unwrap();
        //                                             weirdo ^^^^^^^^^ :P
        users.save().unwrap();
        let file_content = read_locked_file(PASSWD_FILE).unwrap();
        assert_eq!(file_content, concat!(
            "root;$argon2i$m=4096,t=10,p=1$Tnc4UVV0N00$ML9LIOujd3nmAfkAwEcSTMPqakWUF0OUiLWrIy0nGLk;0;0;root;file:/root;file:/bin/ion\n",
            "user;;1000;1000;user;file:/home/user;file:/bin/ion\n",
            "fb;!;7099;7099;FooBar;/home/foob;/bin/zsh\n"
        ));

        {
            let fb = users.get_mut_by_name("fb").unwrap();
            fb.shell = "/bin/fish".to_string(); // That's better
        }
        users.save().unwrap();
        let file_content = read_locked_file(PASSWD_FILE).unwrap();
        assert_eq!(file_content, concat!(
            "root;$argon2i$m=4096,t=10,p=1$Tnc4UVV0N00$ML9LIOujd3nmAfkAwEcSTMPqakWUF0OUiLWrIy0nGLk;0;0;root;file:/root;file:/bin/ion\n",
            "user;;1000;1000;user;file:/home/user;file:/bin/ion\n",
            "fb;!;7099;7099;FooBar;/home/foob;/bin/fish\n"
        ));

        {
            users.remove_by_id(id).unwrap();
        }
        users.save().unwrap();
        let file_content = read_locked_file(PASSWD_FILE).unwrap();
        assert_eq!(file_content, concat!(
            "root;$argon2i$m=4096,t=10,p=1$Tnc4UVV0N00$ML9LIOujd3nmAfkAwEcSTMPqakWUF0OUiLWrIy0nGLk;0;0;root;file:/root;file:/bin/ion\n",
            "user;;1000;1000;user;file:/home/user;file:/bin/ion\n"
        ));
    }

    #[test]
    fn manip_group() {
        let mut groups = get_test_all_groups();
        // NOT testing `get_unique_id`
        let id = 7099;

        groups.add_group("fb", id, &["fb"]).unwrap();
        groups.save().unwrap();
        let file_content = read_locked_file(GROUP_FILE).unwrap();
        assert_eq!(file_content, concat!(
            "root;0;root\n",
            "user;1000;user\n",
            "wheel;1;user,root\n",
            "fb;7099;fb\n"
        ));

        {
            let fb = groups.get_mut_by_name("fb").unwrap();
            fb.users.push("user".to_string());
        }
        groups.save().unwrap();
        let file_content = read_locked_file(GROUP_FILE).unwrap();
        assert_eq!(file_content, concat!(
            "root;0;root\n",
            "user;1000;user\n",
            "wheel;1;user,root\n",
            "fb;7099;fb,user\n"
        ));

        groups.remove_by_id(id).unwrap();
        groups.save().unwrap();
        let file_content = read_locked_file(GROUP_FILE).unwrap();
        assert_eq!(file_content, concat!(
            "root;0;root\n",
            "user;1000;user\n",
            "wheel;1;user,root\n"
        ));
    }

    // *** Goyox's Testing Here
    // This does not use the test files
    fn create_all_users() -> AllUsers {
        let mut root = User {
            user: "root".into(),
            hash: "".into(),
            encoded: None,
            uid: 0,
            gid: 0,
            name: "root".into(),
            home: "/root".into(),
            shell: "/bin/ion".into()
        };
        let mut user = User {
            user: "user".into(),
            hash: "".into(),
            encoded: None,
            uid: 0,
            gid: 0,
            name: "user".into(),
            home: "/home/user".into(),
            shell: "/bin/ion".into()
        };

        root.set_passwd("password").unwrap();
        user.set_passwd("").unwrap();

        let users = vec![root, user];

        users.into_iter().collect()
    }

    #[test]
    fn get_by_name_works() {
        let mut expected_user = User {
            user: "root".into(),
            hash: "".into(),
            encoded: None,
            uid: 0,
            gid: 0,
            name: "root".into(),
            home: "/root".into(),
            shell: "/bin/ion".into()
        };

        expected_user.set_passwd("password").unwrap();
        let all_users = create_all_users();

        assert_eq!(expected_user.gid, all_users.get_by_name("root").unwrap().gid);
    }
}
